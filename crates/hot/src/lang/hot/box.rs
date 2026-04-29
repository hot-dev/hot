//! Hot Box - Container execution for Hot
//!
//! Allows Hot code to spawn containers via `box/start`, which creates
//! a task with `task_type: "container"` and returns `TaskInfo` immediately.
//! The task worker handles actual container execution asynchronously.
//!
//! ## Security
//!
//! - Image denylist (blocks known-bad images; open-by-default with controls)
//! - Network access configurable per-container (default: internet)
//! - Writable root filesystem by default, /tmp (tmpfs) and /data (disk-backed)
//! - Resource limits resolved via 5-tier hierarchy (BoxLimits)
//! - All Linux capabilities dropped, runs as `nobody`

use crate::lang::hot::task::TaskRequest;
use crate::lang::hot::r#type::HotResult;
use crate::queue::Queue;
use crate::val::Val;
use bollard::Docker;
use bollard::container::LogOutput;
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    CreateImageOptionsBuilder, LogsOptionsBuilder, WaitContainerOptionsBuilder,
};
use futures::stream::StreamExt;

// =============================================================================
// Image Denylist
// =============================================================================

const DENIED_IMAGES: &[&str] = &["docker:dind", "docker:latest"];

const DENIED_IMAGE_PREFIXES: &[&str] = &["docker:"];

fn is_image_denied(image: &str) -> bool {
    if DENIED_IMAGES.contains(&image) {
        return true;
    }
    DENIED_IMAGE_PREFIXES
        .iter()
        .any(|prefix| image.starts_with(prefix))
}

// =============================================================================
// ::hot::box/start — start a container task (async, returns TaskInfo)
// =============================================================================

/// Start a Docker container as a task. Returns immediately with TaskInfo.
///
/// Validates the input (image allowlist, field types), creates a TaskRequest
/// with `task_type: "container"`, inserts a task DB row, enqueues to the task
/// queue, and returns `TaskInfo { id, stream-id }`.
///
/// The task worker picks up the request and runs the container via Docker.
/// Results are stored in the task DB record.
pub fn start(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if !is_box_enabled_from_conf(vm.get_conf()) {
        return HotResult::Err(Val::from(
            "::hot::box is disabled. Set hot.box.enabled to true in your configuration.",
        ));
    }

    let input = match validate_and_parse(args) {
        Ok(v) => v,
        Err(result) => return result,
    };

    let ctx = match vm.get_execution_context() {
        Some(ctx) => ctx.clone(),
        None => {
            return HotResult::Err(Val::from("::hot::box/start: no execution context"));
        }
    };

    let env_id = match ctx.env_id {
        Some(id) => id,
        None => {
            return HotResult::Err(Val::from(
                "::hot::box/start: no env_id in execution context",
            ));
        }
    };

    let build_id = match ctx.build_id {
        Some(id) => id,
        None => {
            return HotResult::Err(Val::from(
                "::hot::box/start: no build_id in execution context",
            ));
        }
    };

    let db = match vm.get_database_pool() {
        Some(pool) => pool,
        None => {
            return HotResult::Err(Val::from("::hot::box/start: database not available"));
        }
    };

    let task_queue = match vm.get_task_queue() {
        Some(q) => q,
        None => {
            return HotResult::Err(Val::from("::hot::box/start: task queue not available"));
        }
    };

    let stream_id = ctx.stream_id;
    let task_id = uuid::Uuid::now_v7();

    // CUS quota enforcement
    if let Some(org_id) = ctx.org_id {
        let enforcement_result = tokio::runtime::Handle::current()
            .block_on(async { enforce_cus_quota(&db, &org_id).await });
        if let Err(msg) = enforcement_result {
            return HotResult::Err(Val::from(msg));
        }
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let timeout_ms = input.timeout_secs * 1000 + 30_000;

    // Serialize mounts as an *array* of {container, resource, readonly}
    // rather than the input map so the worker sees the user's declaration
    // order verbatim — mounts must be applied in a deterministic order
    // (e.g. parent dirs before children) and JSON object key order is not
    // guaranteed across serializers.
    let mounts_json: Vec<serde_json::Value> = input
        .mounts
        .iter()
        .map(|m| {
            serde_json::json!({
                "container_path": m.container_path,
                "resource_path": m.resource_path,
                "readonly": m.readonly,
            })
        })
        .collect();

    let args_json = serde_json::json!({
        "image": input.image,
        "size": input.size,
        "cmd": input.cmd,
        "script": input.script,
        "entrypoint": input.entrypoint,
        "env": input.env,
        "timeout_secs": input.timeout_secs,
        "network": input.network,
        "tmp_size_mb": input.tmp_size_mb,
        "disk_size_mb": input.disk_size_mb,
        "memory_mb": input.memory_mb,
        "writable": input.writable,
        "mounts": mounts_json,
    });

    let request = TaskRequest {
        task_id: task_id.to_string(),
        function_name: "::hot::box/start".to_string(),
        args: args_json.clone(),
        stream_id: stream_id.to_string(),
        env_id: env_id.to_string(),
        build_id: build_id.to_string(),
        org_id: ctx.org_id.map(|id| id.to_string()),
        user_id: ctx.user_id.map(|id| id.to_string()),
        project_id: ctx.project_id.map(|id| id.to_string()),
        project_name: ctx.project_name.clone(),
        timeout_ms,
        task_type: "container".to_string(),
        created_at_unix_ms: now_ms,
        origin_run_id: Some(ctx.run_id.to_string()),
    };

    // Ensure the origin run record exists before inserting the task (prevents FK violation)
    tokio::runtime::Handle::current().block_on(async {
        if let Err(e) = crate::db::run::Run::ensure_run_exists(
            &db,
            &ctx.run_id,
            &env_id,
            &stream_id,
            Some(&build_id),
            ctx.run_type_id,
            None,
            &ctx.user_id.unwrap_or(uuid::Uuid::nil()),
            ctx.org_id.as_ref(),
        )
        .await
        {
            tracing::warn!(task_id = %task_id, "::hot::box/start: failed to ensure origin run exists: {}", e);
        }
    });

    // Insert task row and enqueue
    tokio::runtime::Handle::current().block_on(async {
        let args_opt = if args_json.is_null() {
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
            "::hot::box/start",
            args_opt,
            None,
            "container",
            timeout_ms as i64,
            ctx.user_id.as_ref(),
        )
        .await
        {
            tracing::error!(task_id = %task_id, "::hot::box/start: failed to insert task: {}", e);
        }

        if let Err(e) = task_queue.enqueue(request).await {
            tracing::error!(task_id = %task_id, "::hot::box/start: failed to enqueue task: {}", e);
        }
    });

    tracing::debug!(
        task_id = %task_id,
        image = %input.image,
        "box.start.task_created"
    );

    // Return TaskInfo (same shape as ::hot::task/start)
    let result = crate::val!({
        "$type": "::hot::task/TaskInfo",
        "$val": {
            "id": task_id.to_string(),
            "stream-id": stream_id.to_string(),
            "data": Val::Null
        }
    });

    HotResult::Ok(result)
}

// =============================================================================
// Input Validation
// =============================================================================

struct BoxConf {
    image: String,
    size: Option<String>,
    cmd: Option<Vec<String>>,
    script: Option<String>,
    entrypoint: Option<Vec<String>>,
    env: Option<Vec<String>>,
    timeout_secs: u64,
    network: Option<String>,
    tmp_size_mb: Option<u64>,
    disk_size_mb: Option<u64>,
    memory_mb: Option<u64>,
    writable: bool,
    /// Resource subtrees to mount into the container, in declaration order
    /// (the user-facing map has stable iteration so the worker sees the same
    /// order it was specified). Each entry maps a container-side absolute
    /// path to a resource subpath (relative to any of the project's resource
    /// roots — i.e. the same paths that show up under `resources/` in the
    /// bundle).
    mounts: Vec<BoxMount>,
}

#[derive(Clone, Debug)]
struct BoxMount {
    /// Absolute path inside the container (e.g. `/app`).
    container_path: String,
    /// Bundle-relative resource subpath (e.g. `node-app`). The worker
    /// resolves this against the extracted bundle's `resources/` directory.
    resource_path: String,
    /// Whether the bind is read-only inside the container. Default `true`.
    readonly: bool,
}

fn validate_and_parse(args: &[Val]) -> Result<BoxConf, HotResult<Val>> {
    if args.len() != 1 {
        return Err(HotResult::Err(Val::from(
            "::hot::box/start: expected 1 argument",
        )));
    }

    let input = match &args[0] {
        Val::Map(m) => m,
        _ => {
            return Err(HotResult::Err(Val::from("Argument must be a BoxConf map")));
        }
    };

    let image = match input.get(&Val::from("image")) {
        Some(Val::Str(s)) => s.clone(),
        Some(_) => return Err(HotResult::Err(Val::from("'image' field must be a string"))),
        None => return Err(HotResult::Err(Val::from("Missing required 'image' field"))),
    };

    let cmd = input.get(&Val::from("cmd")).and_then(|v| match v {
        Val::Vec(items) => {
            let strings: Result<Vec<String>, &str> = items
                .iter()
                .map(|item| match item {
                    Val::Str(s) => Ok((*s).to_string()),
                    _ => Err("Command args must be strings"),
                })
                .collect();
            strings.ok()
        }
        Val::Null => None,
        _ => None,
    });

    let script = input.get(&Val::from("script")).and_then(|v| match v {
        Val::Str(s) => Some((*s).to_string()),
        Val::Null => None,
        _ => None,
    });

    if cmd.is_some() && script.is_some() {
        return Err(HotResult::Err(Val::from(
            "::hot::box/start: 'cmd' and 'script' are mutually exclusive — use one or the other",
        )));
    }

    let env = input.get(&Val::from("env")).and_then(|v| match v {
        Val::Map(env_map) => {
            let strings: Result<Vec<String>, &str> = env_map
                .iter()
                .map(|(k, v)| match (k, v) {
                    (Val::Str(key), Val::Str(val)) => Ok(format!("{}={}", key, val)),
                    _ => Err("Environment vars must be string key-value pairs"),
                })
                .collect();
            strings.ok()
        }
        Val::Null => None,
        _ => None,
    });

    // Timeout: user can request up to 24 hours (86400s), final clamping via BoxLimits
    let timeout_secs = input
        .get(&Val::from("timeout"))
        .and_then(|v| match v {
            Val::Int(n) => Some((*n as u64).clamp(1, 86_400)),
            _ => None,
        })
        .unwrap_or(60);

    let network = input
        .get(&Val::from("network"))
        .and_then(|v| match v {
            Val::Str(s) => Some(s.to_string()),
            _ => None,
        })
        .or_else(|| Some("internet".to_string()));

    let tmp_size_mb = input.get(&Val::from("tmp-size")).and_then(|v| match v {
        Val::Int(n) if *n > 0 => Some(*n as u64),
        _ => None,
    });

    let disk_size_mb = input.get(&Val::from("disk-size")).and_then(|v| match v {
        Val::Int(n) if *n > 0 => Some(*n as u64),
        _ => None,
    });

    let memory_mb = input.get(&Val::from("memory")).and_then(|v| match v {
        Val::Int(n) if *n > 0 => Some(*n as u64),
        _ => None,
    });

    let valid_sizes = [
        "nano", "micro", "small", "sm", "medium", "md", "large", "lg", "xlarge", "xl", "2xlarge",
        "xxl", "4xlarge",
    ];
    let size = input.get(&Val::from("size")).and_then(|v| match v {
        Val::Str(s) => {
            let s_lower = s.to_lowercase();
            if valid_sizes.contains(&s_lower.as_str()) {
                Some(s_lower)
            } else {
                None
            }
        }
        _ => None,
    });

    let entrypoint = input.get(&Val::from("entrypoint")).and_then(|v| match v {
        Val::Vec(items) => {
            let strings: Result<Vec<String>, &str> = items
                .iter()
                .map(|item| match item {
                    Val::Str(s) => Ok((*s).to_string()),
                    _ => Err("Entrypoint args must be strings"),
                })
                .collect();
            strings.ok()
        }
        Val::Null => None,
        _ => None,
    });

    let writable = input
        .get(&Val::from("writable"))
        .map(|v| !matches!(v, Val::Bool(false)))
        .unwrap_or(true);

    if is_image_denied(&image) {
        return Err(HotResult::Err(Val::from(format!(
            "Image '{}' is not allowed",
            image
        ))));
    }

    let mounts = parse_mounts(input.get(&Val::from("mounts")))
        .map_err(|msg| HotResult::Err(Val::from(msg)))?;

    Ok(BoxConf {
        image: image.to_string(),
        size,
        cmd,
        script,
        entrypoint,
        env,
        timeout_secs,
        network,
        tmp_size_mb,
        disk_size_mb,
        memory_mb,
        writable,
        mounts,
    })
}

/// Parse the optional `mounts:` field of a BoxConf.
///
/// Two accepted shapes for ergonomic specification:
///
/// 1. **Map of container-path → resource-subpath** (read-only by default):
///    ```hot
///    mounts: {"/app": "node-app", "/data/seed": "seeds/v1"}
///    ```
/// 2. **Map of container-path → spec map** (advanced; can opt out of RO):
///    ```hot
///    mounts: {"/app": {path: "node-app", readonly: false}}
///    ```
///
/// `None` (field omitted) and `Val::Null` both yield an empty mounts list.
fn parse_mounts(raw: Option<&Val>) -> Result<Vec<BoxMount>, String> {
    let map = match raw {
        None | Some(Val::Null) => return Ok(Vec::new()),
        Some(Val::Map(m)) => m,
        Some(_) => {
            return Err(
                "'mounts' field must be a Map of container-path → resource-path".to_string(),
            );
        }
    };

    let mut out = Vec::with_capacity(map.len());
    for (k, v) in map.iter() {
        let container_path = match k {
            Val::Str(s) => s.to_string(),
            _ => return Err("'mounts' keys (container paths) must be strings".to_string()),
        };
        validate_container_path(&container_path)?;

        let mount = match v {
            Val::Str(s) => BoxMount {
                container_path: container_path.clone(),
                resource_path: validate_resource_path(s)?,
                readonly: true,
            },
            Val::Map(spec) => {
                let resource_path = match spec.get(&Val::from("path")) {
                    Some(Val::Str(s)) => validate_resource_path(s)?,
                    _ => {
                        return Err(format!(
                            "'mounts.{}.path' must be a non-empty string resource path",
                            container_path
                        ));
                    }
                };
                let readonly = spec
                    .get(&Val::from("readonly"))
                    .map(|v| !matches!(v, Val::Bool(false)))
                    .unwrap_or(true);
                BoxMount {
                    container_path,
                    resource_path,
                    readonly,
                }
            }
            _ => {
                return Err(format!(
                    "'mounts.{}' must be a string (resource path) or Map (spec)",
                    container_path
                ));
            }
        };
        out.push(mount);
    }
    Ok(out)
}

fn validate_container_path(p: &str) -> Result<(), String> {
    if !p.starts_with('/') {
        return Err(format!(
            "container mount path {:?} must be absolute (start with '/')",
            p
        ));
    }
    if p.contains("/../") || p.ends_with("/..") || p == ".." || p.contains('\0') {
        return Err(format!("container mount path {:?} is not allowed", p));
    }
    if p == "/"
        || p == "/proc"
        || p == "/sys"
        || p == "/dev"
        || p == "/data"
        || p == "/tmp"
        || p == "/usr/local/bin"
        || p == "/hot"
        || p == "/hot/sockets"
    {
        return Err(format!(
            "container mount path {:?} clashes with a reserved hot.box mount point",
            p
        ));
    }
    Ok(())
}

fn validate_resource_path(p: &str) -> Result<String, String> {
    let trimmed = p.trim().trim_matches('/').to_string();
    if trimmed.is_empty() {
        return Err("resource mount path must not be empty".to_string());
    }
    if trimmed.contains("..") || trimmed.contains('\0') || trimmed.starts_with('/') {
        return Err(format!(
            "resource mount path {:?} is not allowed (must be a relative subpath \
             with no '..' segments)",
            p
        ));
    }
    Ok(trimmed)
}

// =============================================================================
// CUS Quota Enforcement
// =============================================================================

/// Per-process cache for CUS quota decisions, keyed by org_id.
///
/// `OrgUsageStats::calculate` is a multi-second aggregation. When a busy org
/// fires many `::box/start` calls in a row, we don't want to pay that cost on
/// every invocation — the result is stable on the order of seconds, and the
/// daily CUS budget can't realistically swing far in 30s.
///
/// We cache `Ok(())` decisions for `CUS_QUOTA_CACHE_TTL` and `Err(reason)`
/// decisions for `CUS_QUOTA_DENY_CACHE_TTL` (much shorter so an admin who
/// just bumped the limit unblocks quickly).
static CUS_QUOTA_CACHE: std::sync::OnceLock<
    parking_lot::Mutex<ahash::AHashMap<uuid::Uuid, CachedQuotaDecision>>,
> = std::sync::OnceLock::new();

const CUS_QUOTA_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
const CUS_QUOTA_DENY_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone)]
struct CachedQuotaDecision {
    decided_at: std::time::Instant,
    decision: Result<(), String>,
}

fn cus_quota_cache() -> &'static parking_lot::Mutex<ahash::AHashMap<uuid::Uuid, CachedQuotaDecision>>
{
    CUS_QUOTA_CACHE.get_or_init(|| parking_lot::Mutex::new(ahash::AHashMap::new()))
}

fn cached_quota_decision(org_id: &uuid::Uuid) -> Option<Result<(), String>> {
    let cache = cus_quota_cache().lock();
    let entry = cache.get(org_id)?;
    let ttl = if entry.decision.is_ok() {
        CUS_QUOTA_CACHE_TTL
    } else {
        CUS_QUOTA_DENY_CACHE_TTL
    };
    if entry.decided_at.elapsed() < ttl {
        Some(entry.decision.clone())
    } else {
        None
    }
}

fn store_quota_decision(org_id: &uuid::Uuid, decision: Result<(), String>) {
    let mut cache = cus_quota_cache().lock();
    cache.insert(
        *org_id,
        CachedQuotaDecision {
            decided_at: std::time::Instant::now(),
            decision,
        },
    );
}

/// Check CUS quota before allowing a container task to start.
///
/// - Free plans: hard block when quota is exhausted
/// - Paid plans: allow overage (billed via Stripe)
/// - All plans: block if org-level budget is exceeded
///
/// The result is cached per org for `CUS_QUOTA_CACHE_TTL` to avoid running
/// the multi-second `OrgUsageStats::calculate` aggregation on every
/// `::box/start` call.
async fn enforce_cus_quota(
    db: &crate::db::DatabasePool,
    org_id: &uuid::Uuid,
) -> Result<(), String> {
    if let Some(cached) = cached_quota_decision(org_id) {
        return cached;
    }

    let decision = enforce_cus_quota_uncached(db, org_id).await;
    store_quota_decision(org_id, decision.clone());
    decision
}

async fn enforce_cus_quota_uncached(
    db: &crate::db::DatabasePool,
    org_id: &uuid::Uuid,
) -> Result<(), String> {
    let features = crate::db::features::Features::resolve_for_org(db, org_id).await;
    let cus_limit = features.compute_units_per_month();

    // Unlimited CUS = no enforcement
    if cus_limit < 0 {
        return Ok(());
    }

    // Disabled (0) = block all
    if cus_limit == 0 {
        return Err("Container tasks are not available on your current plan.".to_string());
    }

    let subscription = crate::db::subscription::OrgPlan::get_by_org_id(db, org_id).await;
    let is_free = match subscription.as_ref() {
        Ok(subscription) => crate::db::subscription::Plan::get_by_id(db, &subscription.plan_uuid)
            .await
            .map(|plan| plan.is_free_plan())
            .unwrap_or(false),
        Err(_) => false,
    };

    let period_start = subscription
        .as_ref()
        .ok()
        .and_then(|s| s.current_period_start)
        .unwrap_or_else(chrono::Utc::now);

    let usage = crate::db::subscription::OrgUsageStats::calculate(
        db,
        org_id,
        period_start,
        features.call_retention_days(),
    )
    .await
    .ok();

    let cus_used = usage.as_ref().map(|u| u.compute_units).unwrap_or(0);

    // Check org-level budget cap (applies to all plans)
    let budget = features.compute_units_budget();
    if budget > 0 && cus_used >= budget {
        return Err(format!(
            "Organization compute unit budget ({} CUS) has been reached. Contact your admin to increase the budget.",
            budget
        ));
    }

    // Free plan: hard block at quota
    if is_free && cus_used >= cus_limit {
        return Err(format!(
            "Compute unit quota exhausted ({}/{} CUS). Upgrade your plan for more capacity.",
            cus_used, cus_limit
        ));
    }

    Ok(())
}

// =============================================================================
// Container Execution (called by hot_task_worker, not by Hot code directly)
// =============================================================================

/// Result of running a container
pub struct ContainerRunResult {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
    pub container_id: String,
    pub timed_out: bool,
}

/// Execute a Docker container. Called by `hot_task_worker` for container tasks.
pub async fn run_container_async(
    image: &str,
    cmd: Option<Vec<String>>,
    env: Option<Vec<String>>,
    timeout_secs: u64,
) -> Result<ContainerRunResult, Box<dyn std::error::Error + Send + Sync>> {
    let docker = Docker::connect_with_local_defaults()?;

    // Pull image if not cached
    tracing::debug!(image = %image, "Pulling container image");
    let create_image_options = CreateImageOptionsBuilder::default()
        .from_image(image)
        .build();

    let mut pull_stream = docker.create_image(Some(create_image_options), None, None);
    while let Some(pull_result) = pull_stream.next().await {
        if let Err(e) = pull_result {
            return Err(format!("Failed to pull image '{}': {}", image, e).into());
        }
    }

    let config = ContainerCreateBody {
        image: Some(image.to_string()),
        cmd: cmd.clone(),
        env: env.clone(),
        host_config: Some(HostConfig {
            network_mode: Some("none".to_string()),
            memory: Some(512 * 1024 * 1024),
            memory_swap: Some(512 * 1024 * 1024),
            cpu_quota: Some(50000),
            pids_limit: Some(100i64),
            readonly_rootfs: Some(true),
            security_opt: Some(vec!["no-new-privileges".to_string()]),
            cap_drop: Some(vec!["ALL".to_string()]),
            tmpfs: Some({
                let mut map = std::collections::HashMap::new();
                map.insert("/tmp".to_string(), "size=100m,noexec".to_string());
                map
            }),
            ..Default::default()
        }),
        user: Some("nobody".to_string()),
        ..Default::default()
    };

    tracing::debug!(image = %image, "Creating container");
    let container = docker.create_container(None, config).await?;

    let container_id = container.id.clone();

    tracing::debug!(container_id = %container_id, "Starting container");
    docker.start_container(&container_id, None).await?;

    // Stream logs in real time
    let log_options = LogsOptionsBuilder::default()
        .stdout(true)
        .stderr(true)
        .follow(true)
        .build();

    let mut log_stream = docker.logs(&container_id, Some(log_options));
    let stdout_buf = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
    let stderr_buf = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));

    let stdout_clone = stdout_buf.clone();
    let stderr_clone = stderr_buf.clone();
    let log_handle = tokio::spawn(async move {
        while let Some(log_result) = log_stream.next().await {
            if let Ok(log) = log_result {
                match log {
                    LogOutput::StdOut { message } => {
                        stdout_clone
                            .lock()
                            .await
                            .push_str(&String::from_utf8_lossy(&message));
                    }
                    LogOutput::StdErr { message } => {
                        stderr_clone
                            .lock()
                            .await
                            .push_str(&String::from_utf8_lossy(&message));
                    }
                    _ => {}
                }
            }
        }
    });

    // Wait for completion with timeout
    let wait_options = WaitContainerOptionsBuilder::default()
        .condition("not-running")
        .build();

    let wait_future = async {
        let mut wait_stream = docker.wait_container(&container_id, Some(wait_options));
        while let Some(_wait_result) = wait_stream.next().await {}
    };

    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), wait_future).await {
        Ok(()) => {
            let exit_code = match docker.inspect_container(&container_id, None).await {
                Ok(info) => info.state.and_then(|s| s.exit_code).unwrap_or(0),
                Err(e) => {
                    tracing::warn!(container_id = %container_id, error = %e, "Failed to inspect container for exit code");
                    0
                }
            };

            tracing::debug!(container_id = %container_id, exit_code = %exit_code, "Container exited");

            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), log_handle).await;

            let stdout = stdout_buf.lock().await.clone();
            let stderr = stderr_buf.lock().await.clone();

            tracing::debug!(container_id = %container_id, "Removing container");
            docker.remove_container(&container_id, None).await.ok();

            Ok(ContainerRunResult {
                exit_code,
                stdout,
                stderr,
                container_id,
                timed_out: false,
            })
        }
        Err(_) => {
            let stdout = stdout_buf.lock().await.clone();
            let stderr = stderr_buf.lock().await.clone();

            tracing::warn!(
                container_id = %container_id,
                timeout_secs = timeout_secs,
                stdout_len = stdout.len(),
                stderr_len = stderr.len(),
                "Container timed out, capturing partial output and killing"
            );

            docker.kill_container(&container_id, None).await.ok();
            docker.remove_container(&container_id, None).await.ok();

            Ok(ContainerRunResult {
                exit_code: -1,
                stdout,
                stderr,
                container_id,
                timed_out: true,
            })
        }
    }
}

// =============================================================================
// Utility Functions (allowed-images, mode, stats)
// =============================================================================

/// Get the list of denied images (images that are blocked).
pub fn allowed_images(_args: &[Val]) -> HotResult<Val> {
    let mut result = indexmap::IndexMap::new();
    let denied: Vec<Val> = DENIED_IMAGES
        .iter()
        .map(|s| Val::from(s.to_string()))
        .collect();
    let prefixes: Vec<Val> = DENIED_IMAGE_PREFIXES
        .iter()
        .map(|s| Val::from(s.to_string()))
        .collect();
    result.insert(Val::from("policy"), Val::from("open"));
    result.insert(Val::from("denied"), Val::Vec(denied));
    result.insert(Val::from("denied-prefixes"), Val::Vec(prefixes));
    HotResult::Ok(Val::Map(Box::new(result)))
}

/// Check whether box (container execution) is enabled via conf.
fn is_box_enabled_from_conf(conf: &Val) -> bool {
    if let Some(val) = conf.get("box.enabled") {
        match val {
            Val::Bool(b) => return b,
            Val::Str(s) => {
                return matches!(s.to_lowercase().as_str(), "true" | "1");
            }
            _ => {}
        }
    }
    false
}

/// Return whether box is enabled as a Bool.
pub fn enabled(vm: &mut crate::lang::runtime::vm::VirtualMachine, _args: &[Val]) -> HotResult<Val> {
    HotResult::Ok(Val::from(is_box_enabled_from_conf(vm.get_conf())))
}

/// Get available container size presets.
pub fn sizes(_args: &[Val]) -> HotResult<Val> {
    let sizes: Vec<Val> = [
        ("nano", 64, 10, 32, 256, 60, 0.25),
        ("micro", 128, 25, 64, 512, 60, 0.5),
        ("small", 256, 25, 128, 1024, 60, 1.0),
        ("medium", 512, 50, 256, 5120, 300, 2.0),
        ("large", 1024, 75, 500, 10240, 600, 4.0),
        ("xlarge", 2048, 100, 1024, 20480, 1800, 8.0),
        ("2xlarge", 4096, 100, 2048, 51200, 3600, 16.0),
        ("4xlarge", 8192, 100, 4096, 51200, 7200, 32.0),
    ]
    .iter()
    .map(|(name, mem, cpu, tmp, disk, timeout, multiplier)| {
        crate::val!({
            "name": *name,
            "memory-mb": *mem as i64,
            "cpu-pct": *cpu as i64,
            "tmp-mb": *tmp as i64,
            "disk-mb": *disk as i64,
            "timeout-secs": *timeout as i64,
            "cus-multiplier": *multiplier
        })
    })
    .collect();
    HotResult::Ok(Val::Vec(sizes))
}

/// Get container execution statistics.
pub fn stats(vm: &mut crate::lang::runtime::vm::VirtualMachine, _args: &[Val]) -> HotResult<Val> {
    let mut result = indexmap::IndexMap::new();
    result.insert(
        Val::from("enabled"),
        Val::from(is_box_enabled_from_conf(vm.get_conf())),
    );
    HotResult::Ok(Val::Map(Box::new(result)))
}

/// Get remaining container task quota for the current org.
pub fn quota(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if !args.is_empty() {
        return HotResult::Err(Val::from("::hot::box/quota: expected 0 arguments"));
    }

    let ctx = match vm.get_execution_context() {
        Some(ctx) => ctx.clone(),
        None => return HotResult::Err(Val::from("::hot::box/quota: no execution context")),
    };

    let org_id = match ctx.org_id {
        Some(id) => id,
        None => return HotResult::Err(Val::from("::hot::box/quota: no org_id in context")),
    };

    let db = match vm.get_database_pool() {
        Some(pool) => pool,
        None => return HotResult::Err(Val::from("::hot::box/quota: database not available")),
    };

    let result = tokio::runtime::Handle::current().block_on(async {
        let features = crate::db::features::Features::resolve_for_org(&db, &org_id).await;

        let subscription = crate::db::subscription::OrgPlan::get_by_org_id(&db, &org_id).await;

        let is_free = match subscription.as_ref() {
            Ok(subscription) => {
                crate::db::subscription::Plan::get_by_id(&db, &subscription.plan_uuid)
                    .await
                    .map(|plan| plan.is_free_plan())
                    .unwrap_or(false)
            }
            Err(_) => false,
        };

        let period_start = subscription
            .as_ref()
            .ok()
            .and_then(|s| s.current_period_start)
            .unwrap_or_else(chrono::Utc::now);

        let usage = crate::db::subscription::OrgUsageStats::calculate(
            &db,
            &org_id,
            period_start,
            features.call_retention_days(),
        )
        .await
        .ok();

        let minutes_remaining = {
            let limit = features.task_minutes_per_month();
            if limit < 0 {
                -1i64
            } else {
                let used_ms = usage.as_ref().map(|u| u.task_duration_ms).unwrap_or(0);
                let used_min = used_ms / 60_000;
                if is_free {
                    (limit as i64).saturating_sub(used_min).max(0)
                } else {
                    // Paid plans: can go negative (overage)
                    (limit as i64) - used_min
                }
            }
        };

        let concurrent_remaining = {
            let limit = features.box_concurrent_tasks();
            if limit < 0 {
                -1i64
            } else {
                // Heartbeat-aware count: matches the worker-side enforcement
                // in `hot_task_worker::process_task` so the UI doesn't show
                // "0 remaining" while the periodic reaper is still cleaning
                // up zombies from a dead worker.
                let running = crate::db::Task::count_running_containers_for_org(
                    &db,
                    &org_id,
                    crate::db::task::QUOTA_HEARTBEAT_FRESH_SECS,
                )
                .await
                .unwrap_or(0);
                (limit).saturating_sub(running).max(0)
            }
        };

        let compute_units_used = usage.as_ref().map(|u| u.compute_units).unwrap_or(0);

        let compute_units_remaining = {
            let limit = features.compute_units_per_month();
            if limit < 0 {
                -1i64
            } else {
                let remaining = limit.saturating_sub(compute_units_used);
                if is_free {
                    remaining.max(0)
                } else {
                    limit - compute_units_used
                }
            }
        };

        let overage = if is_free {
            false
        } else {
            minutes_remaining < 0 || compute_units_remaining < 0
        };

        let mut map = indexmap::IndexMap::new();
        map.insert(Val::from("$type"), Val::from("::hot::box/BoxQuota"));
        let mut val_map = indexmap::IndexMap::new();
        val_map.insert(Val::from("minutes-remaining"), Val::Int(minutes_remaining));
        val_map.insert(
            Val::from("compute-units-remaining"),
            Val::Int(compute_units_remaining),
        );
        val_map.insert(
            Val::from("compute-units-used"),
            Val::Int(compute_units_used),
        );
        val_map.insert(
            Val::from("concurrent-remaining"),
            Val::Int(concurrent_remaining),
        );
        val_map.insert(Val::from("overage"), Val::Bool(overage));
        map.insert(Val::from("$val"), Val::Map(Box::new(val_map)));
        Val::Map(Box::new(map))
    });

    HotResult::Ok(result)
}

/// Get the resolved container resource limits for the current org's plan.
///
/// Returns a typed BoxLimits map with memory-mb, cpu-pct, tmp-mb, disk-mb,
/// timeout-secs, and network fields, reflecting what the org is actually
/// allowed to use (after plan + org feature resolution).
pub fn limits(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if !args.is_empty() {
        return HotResult::Err(Val::from("::hot::box/limits: expected 0 arguments"));
    }

    let ctx = match vm.get_execution_context() {
        Some(ctx) => ctx.clone(),
        None => return HotResult::Err(Val::from("::hot::box/limits: no execution context")),
    };

    let org_id = match ctx.org_id {
        Some(id) => id,
        None => return HotResult::Err(Val::from("::hot::box/limits: no org_id in context")),
    };

    let db = match vm.get_database_pool() {
        Some(pool) => pool,
        None => return HotResult::Err(Val::from("::hot::box/limits: database not available")),
    };

    let result = tokio::runtime::Handle::current().block_on(async {
        let features = crate::db::features::Features::resolve_for_org(&db, &org_id).await;

        let memory_mb = features.box_memory_mb();
        let cpu_quota = features.box_cpu_quota();
        let tmp_size_mb = features.box_tmp_size_mb();
        let disk_size_mb = features.box_disk_size_mb();
        let timeout_secs = features.box_timeout_secs();
        let network = features.box_network_allowed();

        // Convert cpu_quota (Docker microseconds per 100ms period) to percentage
        // 100_000 = 100%, 50_000 = 50%, 10_000 = 10%
        let cpu_pct = if cpu_quota < 0 {
            100i64 // unlimited = 100%
        } else {
            (cpu_quota / 1000).min(100)
        };

        let mut map = indexmap::IndexMap::new();
        map.insert(Val::from("$type"), Val::from("::hot::box/BoxLimits"));
        let mut val_map = indexmap::IndexMap::new();
        val_map.insert(Val::from("memory-mb"), Val::Int(memory_mb));
        val_map.insert(Val::from("cpu-pct"), Val::Int(cpu_pct));
        val_map.insert(Val::from("tmp-mb"), Val::Int(tmp_size_mb));
        val_map.insert(Val::from("disk-mb"), Val::Int(disk_size_mb));
        val_map.insert(Val::from("timeout-secs"), Val::Int(timeout_secs));
        val_map.insert(Val::from("network"), Val::Bool(network));
        map.insert(Val::from("$val"), Val::Map(Box::new(val_map)));
        Val::Map(Box::new(map))
    });

    HotResult::Ok(result)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_box_enabled_default() {
        let empty_conf = Val::Null;
        assert!(!is_box_enabled_from_conf(&empty_conf));
    }

    #[test]
    fn test_box_enabled_from_conf_true() {
        let conf = crate::val!({"box": {"enabled": true}});
        assert!(is_box_enabled_from_conf(&conf));
    }

    #[test]
    fn test_box_enabled_from_conf_false() {
        let conf = crate::val!({"box": {"enabled": false}});
        assert!(!is_box_enabled_from_conf(&conf));
    }

    #[test]
    fn test_box_enabled_from_conf_string() {
        let conf = crate::val!({"box": {"enabled": "true"}});
        assert!(is_box_enabled_from_conf(&conf));
        let conf = crate::val!({"box": {"enabled": "false"}});
        assert!(!is_box_enabled_from_conf(&conf));
    }

    #[test]
    fn test_denylist_blocks_docker_dind() {
        assert!(is_image_denied("docker:dind"));
        assert!(is_image_denied("docker:latest"));
        assert!(is_image_denied("docker:24.0"));
    }

    #[test]
    fn test_denylist_allows_normal_images() {
        assert!(!is_image_denied("alpine:latest"));
        assert!(!is_image_denied("python:3.13-alpine"));
        assert!(!is_image_denied("node:22-alpine"));
        assert!(!is_image_denied("custom-image:v1"));
    }

    fn validate_start(args: &[Val]) -> HotResult<Val> {
        match validate_and_parse(args) {
            Ok(_) => HotResult::Ok(Val::from("validated")),
            Err(result) => result,
        }
    }

    #[test]
    fn test_start_validates_input_type() {
        let result = validate_start(&[Val::from("not a map")]);
        match result {
            HotResult::Err(val) => {
                let msg = val.to_string();
                assert!(
                    msg.contains("must be a") || msg.contains("map"),
                    "Expected error about map type, got: {}",
                    msg
                );
            }
            HotResult::Ok(_) => panic!("Expected Err for invalid input type"),
        }
    }

    #[test]
    fn test_start_requires_image_field() {
        let input = crate::val!({ "cmd": ["echo", "hello"] });
        let result = validate_start(&[input]);
        match result {
            HotResult::Err(val) => {
                let msg = val.to_string();
                assert!(msg.contains("image"), "Expected error about image field");
            }
            HotResult::Ok(_) => panic!("Expected Err when image is missing"),
        }
    }

    #[test]
    fn test_start_rejects_denied_image() {
        let input = crate::val!({ "image": "docker:dind" });
        let result = validate_start(&[input]);
        match result {
            HotResult::Err(val) => {
                let msg = val.to_string();
                assert!(
                    msg.contains("not allowed"),
                    "Expected denial error, got: {}",
                    msg
                );
            }
            HotResult::Ok(_) => panic!("Expected Err for denied image"),
        }
    }

    #[test]
    fn test_start_accepts_valid_input() {
        let input = crate::val!({
            "image": "alpine:latest",
            "cmd": ["echo", "hello"],
            "timeout": 30
        });
        let result = validate_start(&[input]);
        assert!(
            matches!(result, HotResult::Ok(_)),
            "Expected Ok for valid input"
        );
    }

    #[test]
    fn test_start_accepts_custom_image() {
        let input = crate::val!({
            "image": "myorg/myimage:v2",
            "cmd": ["run"]
        });
        let result = validate_start(&[input]);
        assert!(
            matches!(result, HotResult::Ok(_)),
            "Expected Ok for custom image (open policy)"
        );
    }

    #[test]
    fn test_start_defaults_timeout_to_60() {
        let input = crate::val!({ "image": "alpine:latest" });
        let parsed = validate_and_parse(&[input]).unwrap();
        assert_eq!(parsed.timeout_secs, 60);
    }

    #[test]
    fn test_start_clamps_timeout_to_24h() {
        let input = crate::val!({ "image": "alpine:latest", "timeout": 999999 });
        let parsed = validate_and_parse(&[input]).unwrap();
        assert_eq!(parsed.timeout_secs, 86_400);
    }

    #[test]
    fn test_start_parses_new_fields() {
        let input = crate::val!({
            "image": "alpine:latest",
            "network": "internet",
            "tmp-size": 1024,
            "disk-size": 10240,
            "memory": 2048
        });
        let parsed = validate_and_parse(&[input]).unwrap();
        assert_eq!(parsed.network.as_deref(), Some("internet"));
        assert_eq!(parsed.tmp_size_mb, Some(1024));
        assert_eq!(parsed.disk_size_mb, Some(10240));
        assert_eq!(parsed.memory_mb, Some(2048));
    }

    #[test]
    fn test_start_new_fields_defaults() {
        let input = crate::val!({ "image": "alpine:latest" });
        let parsed = validate_and_parse(&[input]).unwrap();
        assert_eq!(parsed.network.as_deref(), Some("internet"));
        assert!(parsed.writable);
        assert!(parsed.tmp_size_mb.is_none());
        assert!(parsed.disk_size_mb.is_none());
        assert!(parsed.memory_mb.is_none());
        assert!(parsed.size.is_none());
    }

    #[test]
    fn test_mounts_default_empty() {
        let input = crate::val!({ "image": "alpine:latest" });
        let parsed = validate_and_parse(&[input]).unwrap();
        assert!(parsed.mounts.is_empty());
    }

    #[test]
    fn test_mounts_simple_string_form_is_readonly_by_default() {
        let input = crate::val!({
            "image": "alpine:latest",
            "mounts": {"/app": "node-app", "/data/seed": "seeds/v1"}
        });
        let parsed = validate_and_parse(&[input]).unwrap();
        assert_eq!(parsed.mounts.len(), 2);
        let app = parsed
            .mounts
            .iter()
            .find(|m| m.container_path == "/app")
            .unwrap();
        assert_eq!(app.resource_path, "node-app");
        assert!(app.readonly);
        let seed = parsed
            .mounts
            .iter()
            .find(|m| m.container_path == "/data/seed")
            .unwrap();
        assert_eq!(seed.resource_path, "seeds/v1");
        assert!(seed.readonly);
    }

    #[test]
    fn test_mounts_spec_form_can_disable_readonly() {
        let input = crate::val!({
            "image": "alpine:latest",
            "mounts": {"/app": {"path": "node-app", "readonly": false}}
        });
        let parsed = validate_and_parse(&[input]).unwrap();
        assert_eq!(parsed.mounts.len(), 1);
        assert_eq!(parsed.mounts[0].container_path, "/app");
        assert_eq!(parsed.mounts[0].resource_path, "node-app");
        assert!(!parsed.mounts[0].readonly);
    }

    #[test]
    fn test_mounts_strips_resource_leading_and_trailing_slashes() {
        let input = crate::val!({
            "image": "alpine:latest",
            "mounts": {"/app": "node-app/"}
        });
        let parsed = validate_and_parse(&[input]).unwrap();
        assert_eq!(parsed.mounts[0].resource_path, "node-app");
    }

    #[test]
    fn test_mounts_rejects_relative_container_path() {
        let input = crate::val!({
            "image": "alpine:latest",
            "mounts": {"app": "node-app"}
        });
        let err = match validate_and_parse(&[input]) {
            Ok(_) => panic!("expected error"),
            Err(HotResult::Err(v)) => v.to_string(),
            Err(_) => panic!("unexpected"),
        };
        assert!(err.contains("absolute"), "{}", err);
    }

    #[test]
    fn test_mounts_rejects_traversal_in_resource_path() {
        let input = crate::val!({
            "image": "alpine:latest",
            "mounts": {"/app": "../escape"}
        });
        let err = match validate_and_parse(&[input]) {
            Ok(_) => panic!("expected error"),
            Err(HotResult::Err(v)) => v.to_string(),
            Err(_) => panic!("unexpected"),
        };
        assert!(err.contains("not allowed"), "{}", err);
    }

    #[test]
    fn test_mounts_rejects_reserved_container_path() {
        for reserved in ["/", "/data", "/proc", "/sys", "/tmp", "/usr/local/bin"] {
            let input = crate::val!({
                "image": "alpine:latest",
                "mounts": {reserved: "node-app"}
            });
            let err = match validate_and_parse(&[input]) {
                Ok(_) => panic!("expected error for {}", reserved),
                Err(HotResult::Err(v)) => v.to_string(),
                Err(_) => panic!("unexpected"),
            };
            assert!(
                err.contains("reserved"),
                "expected reserved-path error for {}: {}",
                reserved,
                err
            );
        }
    }

    #[test]
    fn test_mounts_rejects_non_map() {
        let input = crate::val!({
            "image": "alpine:latest",
            "mounts": "node-app"
        });
        let err = match validate_and_parse(&[input]) {
            Ok(_) => panic!("expected error"),
            Err(HotResult::Err(v)) => v.to_string(),
            Err(_) => panic!("unexpected"),
        };
        assert!(err.contains("Map"), "{}", err);
    }

    #[test]
    fn test_mounts_rejects_empty_resource_path() {
        let input = crate::val!({
            "image": "alpine:latest",
            "mounts": {"/app": ""}
        });
        let err = match validate_and_parse(&[input]) {
            Ok(_) => panic!("expected error"),
            Err(HotResult::Err(v)) => v.to_string(),
            Err(_) => panic!("unexpected"),
        };
        assert!(err.contains("not be empty"), "{}", err);
    }

    #[test]
    fn test_start_parses_size_field() {
        let input = crate::val!({ "image": "alpine:latest", "size": "nano" });
        let parsed = validate_and_parse(&[input]).unwrap();
        assert_eq!(parsed.size.as_deref(), Some("nano"));
    }

    #[test]
    fn test_start_parses_size_aliases() {
        for (alias, expected) in [("sm", "sm"), ("md", "md"), ("lg", "lg"), ("xl", "xl")] {
            let input = crate::val!({ "image": "alpine:latest", "size": alias });
            let parsed = validate_and_parse(&[input]).unwrap();
            assert_eq!(parsed.size.as_deref(), Some(expected));
        }
    }

    #[test]
    fn test_start_ignores_invalid_size() {
        let input = crate::val!({ "image": "alpine:latest", "size": "invalid" });
        let parsed = validate_and_parse(&[input]).unwrap();
        assert!(parsed.size.is_none());
    }

    #[test]
    fn test_sizes_returns_all_presets() {
        let result = sizes(&[]);
        match result {
            HotResult::Ok(Val::Vec(items)) => {
                assert_eq!(items.len(), 8);
            }
            _ => panic!("Expected Ok(Vec)"),
        }
    }

    #[test]
    fn test_allowed_images_returns_policy_map() {
        let result = allowed_images(&[]);
        match result {
            HotResult::Ok(Val::Map(m)) => {
                assert!(m.contains_key(&Val::from("policy")));
                assert!(m.contains_key(&Val::from("denied")));
            }
            _ => panic!("Expected Ok(Map)"),
        }
    }

    // stats(), quota(), limits() require VM access with execution context and DB —
    // tested via Hot-level integration tests
}

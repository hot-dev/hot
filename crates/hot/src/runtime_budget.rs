use crate::val::Val;
use std::num::NonZeroUsize;

/// Default hard ceiling on the derived Postgres pool size when no explicit
/// `worker.db-max-connections` is configured. Prevents large hosts from
/// deriving connection counts that exhaust the database's connection limit.
const DEFAULT_DB_MAX_CONNECTIONS: usize = 50;

/// Process memory (MB) held back from the task-worker budget for the runtime
/// itself before dividing the rest between code VMs and containers.
const TASK_PROCESS_RESERVE_MB: u64 = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedConcurrency {
    pub requested: usize,
    pub resolved: usize,
    pub cpu_limit: usize,
    pub memory_limit: Option<usize>,
    pub memory_limit_mb: Option<u64>,
    pub explicit: bool,
    pub shared_process: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedContainerConcurrency {
    pub requested: usize,
    pub resolved: usize,
    pub memory_limit: usize,
    pub disk_limit: usize,
    pub memory_budget_mb: u64,
    pub disk_budget_mb: u64,
    pub explicit: bool,
    pub recovery_reserved_slots: usize,
    pub backend: String,
}

pub fn derive_worker_vm_concurrency(conf: &Val, requested: usize) -> DerivedConcurrency {
    let requested = requested.max(1);
    let shared_process = conf.get_bool_or_default("worker.shared-process", false);
    let memory_limit_mb = detected_memory_limit_mb();
    let vm_memory_mb = conf.get_int_or_default("worker.vm-memory-mb", 256).max(1) as u64;
    let reserved_memory_mb = conf
        .get_int_or_default("worker.reserved-memory-mb", 512)
        .max(0) as u64;
    let memory_limit = memory_limit_mb.map(|limit| {
        limit
            .saturating_sub(reserved_memory_mb)
            .checked_div(vm_memory_mb)
            .unwrap_or(1)
            .max(1) as usize
    });

    if let Some(explicit) = configured_usize(conf, "worker.vm-concurrency") {
        let mut resolved = explicit.min(requested);
        if let Some(memory_limit) = memory_limit {
            resolved = resolved.min(memory_limit);
        }

        return DerivedConcurrency {
            requested,
            resolved: resolved.max(1),
            cpu_limit: requested,
            memory_limit,
            memory_limit_mb,
            explicit: true,
            shared_process,
        };
    }

    let cpu_limit = effective_cpu_parallelism(shared_process).max(1);

    let mut resolved = requested.min(cpu_limit);
    if let Some(memory_limit) = memory_limit {
        resolved = resolved.min(memory_limit);
    }

    DerivedConcurrency {
        requested,
        resolved: resolved.max(1),
        cpu_limit,
        memory_limit,
        memory_limit_mb,
        explicit: false,
        shared_process,
    }
}

pub fn derive_task_code_concurrency(conf: &Val, requested: usize) -> DerivedConcurrency {
    let requested = requested.max(1);
    let shared_process = conf.get_bool_or_default("worker.shared-process", false);
    let cpu_limit = effective_cpu_parallelism(shared_process).max(1);
    let vm_memory_mb = conf
        .get_int_or_default("task.code-vm-memory-mb", 256)
        .max(1) as u64;
    let ceiling_mb = task_memory_ceiling_mb(conf);
    let usable_mb = task_usable_memory_mb(conf, ceiling_mb);
    // Code VMs and containers draw from disjoint shares of usable memory so the
    // two budgets never double-count the same RAM.
    let code_share_mb = usable_mb.saturating_mul(code_memory_percent(conf)) / 100;
    let memory_limit_mb = Some(ceiling_mb);
    let memory_limit = Some(code_share_mb.checked_div(vm_memory_mb).unwrap_or(1).max(1) as usize);

    let mut resolved = requested.min(cpu_limit);
    if let Some(memory_limit) = memory_limit {
        resolved = resolved.min(memory_limit);
    }

    DerivedConcurrency {
        requested,
        resolved: resolved.max(1),
        cpu_limit,
        memory_limit,
        memory_limit_mb,
        explicit: false,
        shared_process,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn derive_task_container_concurrency(
    conf: &Val,
    legacy_requested: usize,
    worker_memory_mb: u64,
    worker_disk_mb: u64,
    default_container_memory_mb: u64,
    default_container_disk_mb: u64,
    default_container_tmp_mb: u64,
    backend: impl Into<String>,
) -> DerivedContainerConcurrency {
    let explicit = configured_usize(conf, "task.container-max-concurrent");
    let requested = explicit.unwrap_or(legacy_requested.max(1)).max(1);
    let recovery_reserved_slots = conf
        .get_int_or_default("task.recovery-reserved-slots", 1)
        .max(0) as usize;
    let reserved_disk_mb = conf
        .get_int_or_default("task.container-reserved-disk-mb", 10_240)
        .max(0) as u64
        * recovery_reserved_slots.max(1) as u64;

    let detected_memory_mb = detected_memory_limit_mb();
    let memory_ceiling_mb = detected_memory_mb
        .map(|limit| limit.min(worker_memory_mb.max(1)))
        .unwrap_or_else(|| worker_memory_mb.max(1));
    let usable_mb = task_usable_memory_mb(conf, memory_ceiling_mb);
    // Containers take the complement of the code-VM memory share so the two
    // resource classes do not over-commit the same RAM.
    let container_percent = 100u64.saturating_sub(code_memory_percent(conf));
    let memory_budget_mb = (usable_mb.saturating_mul(container_percent) / 100).max(1);
    let disk_budget_mb = worker_disk_mb
        .max(1)
        .saturating_sub(reserved_disk_mb)
        .max(1);
    let per_container_memory_mb = default_container_memory_mb
        .saturating_add(default_container_tmp_mb)
        .max(1);
    let per_container_disk_mb = default_container_disk_mb.max(1);
    let memory_limit = memory_budget_mb
        .checked_div(per_container_memory_mb)
        .unwrap_or(1)
        .max(1) as usize;
    let disk_limit = disk_budget_mb
        .checked_div(per_container_disk_mb)
        .unwrap_or(1)
        .max(1) as usize;
    let resolved = requested.min(memory_limit).min(disk_limit).max(1);

    DerivedContainerConcurrency {
        requested,
        resolved,
        memory_limit,
        disk_limit,
        memory_budget_mb,
        disk_budget_mb,
        explicit: explicit.is_some(),
        recovery_reserved_slots,
        backend: backend.into(),
    }
}

pub fn derive_postgres_pool_connections(conf: &Val) -> u32 {
    let worker_budget = configured_usize(conf, "worker.threads")
        .map(|requested| derive_worker_vm_concurrency(conf, requested).resolved)
        .unwrap_or(0);
    let task_code_budget = if conf.get("task").is_some() {
        let requested = conf
            .get_int_or_default("task.code-max-concurrent", 500)
            .max(1) as usize;
        derive_task_code_concurrency(conf, requested).resolved
    } else {
        0
    };
    let reserved = conf
        .get_int_or_default("worker.db-reserved-connections", 4)
        .max(1) as usize;
    let capacity_budget = worker_budget.max(task_code_budget);
    let derived = capacity_budget.saturating_add(reserved).max(10);

    // Clamp so large hosts don't derive a pool that exhausts the database's
    // global connection limit. An explicit `worker.db-max-connections` raises
    // or lowers the ceiling; otherwise a conservative default cap applies.
    let explicit_ceiling_floor = if conf.get_bool_or_default("scheduler.singleton", false)
        && conf.get("scheduler").is_some()
    {
        // The scheduler holds one Postgres connection for the lifetime of
        // its session-level advisory lock. Keep at least one other
        // connection available for normal scheduler DB work.
        2
    } else {
        1
    };
    let ceiling = configured_usize(conf, "worker.db-max-connections")
        .map(|c| c.max(explicit_ceiling_floor))
        .unwrap_or(DEFAULT_DB_MAX_CONNECTIONS);

    derived.min(ceiling).min(u32::MAX as usize) as u32
}

/// Size the SQLite connection pool. SQLite in WAL mode allows many concurrent
/// readers but serializes writers at the storage layer, so the pool is sized
/// for *read* concurrency (worker execution slots) plus a small write headroom.
/// `worker.local-write-concurrency` adds headroom rather than capping the whole
/// pool, and a floor of 10 preserves prior behavior for default deployments.
pub fn derive_sqlite_pool_connections(conf: &Val) -> u32 {
    let read_capacity = configured_usize(conf, "worker.threads")
        .map(|requested| derive_worker_vm_concurrency(conf, requested).resolved)
        .unwrap_or(0);
    let write_headroom = conf
        .get_int_or_default("worker.local-write-concurrency", 1)
        .max(1) as usize;

    read_capacity
        .saturating_add(write_headroom)
        .max(10)
        .min(u32::MAX as usize) as u32
}

fn code_memory_percent(conf: &Val) -> u64 {
    conf.get_int_or_default("task.code-memory-percent", 50)
        .clamp(0, 100) as u64
}

/// Memory ceiling (MB) available to a task worker: the smaller of the detected
/// cgroup limit and the operator-declared `task.worker-memory-mb`.
fn task_memory_ceiling_mb(conf: &Val) -> u64 {
    let configured = conf
        .get_int_or_default("task.worker-memory-mb", 8192)
        .max(1) as u64;
    match detected_memory_limit_mb() {
        Some(detected) => detected.min(configured),
        None => configured,
    }
}

/// Memory (MB) that may be split between code VMs and containers after holding
/// back process overhead and a recovery reserve for adopting in-flight
/// containers on restart.
fn task_usable_memory_mb(conf: &Val, ceiling_mb: u64) -> u64 {
    let container_reserve_mb = conf
        .get_int_or_default("task.container-reserved-memory-mb", 512)
        .max(0) as u64;
    let recovery_slots = conf
        .get_int_or_default("task.recovery-reserved-slots", 1)
        .max(0) as u64;
    let recovery_reserve_mb = container_reserve_mb.saturating_mul(recovery_slots.max(1));
    ceiling_mb
        .saturating_sub(TASK_PROCESS_RESERVE_MB)
        .saturating_sub(recovery_reserve_mb)
        .max(1)
}

fn configured_usize(conf: &Val, path: &str) -> Option<usize> {
    let value = conf.get(path)?;
    match value {
        Val::Int(i) if i > 0 => Some(i as usize),
        Val::Str(s) => {
            let s = s.trim();
            if s.eq_ignore_ascii_case("auto") || s.is_empty() || s == "null" {
                None
            } else {
                s.parse::<usize>().ok().filter(|n| *n > 0)
            }
        }
        _ => None,
    }
}

fn effective_cpu_parallelism(shared_process: bool) -> usize {
    let cpus = cgroup_cpu_quota()
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(NonZeroUsize::get)
                .unwrap_or(1)
        })
        .max(1);

    if shared_process {
        cpus.saturating_div(2).max(1)
    } else {
        cpus
    }
}

fn cgroup_cpu_quota() -> Option<usize> {
    if let Ok(cpu_max) = std::fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        let mut parts = cpu_max.split_whitespace();
        let quota = parts.next()?;
        let period = parts.next()?;
        if quota != "max" {
            let quota = quota.parse::<u64>().ok()?;
            let period = period.parse::<u64>().ok()?.max(1);
            return Some(quota.div_ceil(period).max(1) as usize);
        }
    }

    let quota = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us")
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()?;
    if quota <= 0 {
        return None;
    }
    let period = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us")
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?
        .max(1);
    Some((quota as u64).div_ceil(period).max(1) as usize)
}

fn detected_memory_limit_mb() -> Option<u64> {
    let bytes = cgroup_memory_limit_bytes()?;
    Some((bytes / (1024 * 1024)).max(1))
}

fn cgroup_memory_limit_bytes() -> Option<u64> {
    for path in [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ] {
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        let trimmed = raw.trim();
        if trimmed == "max" {
            return None;
        }
        let bytes = trimmed.parse::<u64>().ok()?;
        if bytes > 0 && bytes < (1u64 << 60) {
            return Some(bytes);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::val;

    #[test]
    fn explicit_worker_vm_concurrency_never_raises_requested_threads() {
        let conf = val!({
            "worker": {
                "vm-concurrency": 8i64,
                "shared-process": false,
            },
        });

        let budget = derive_worker_vm_concurrency(&conf, 4);

        assert_eq!(budget.resolved, 4);
        assert!(budget.explicit);
    }

    #[test]
    fn auto_worker_vm_concurrency_keeps_at_least_one_slot() {
        let conf = val!({
            "worker": {
                "vm-concurrency": "auto",
                "vm-memory-mb": 256i64,
                "reserved-memory-mb": 512i64,
                "shared-process": true,
            },
        });

        let budget = derive_worker_vm_concurrency(&conf, 4);

        assert!(budget.resolved >= 1);
        assert!(budget.resolved <= 4);
        assert!(!budget.explicit);
        assert!(budget.shared_process);
    }

    #[test]
    fn task_code_concurrency_caps_large_default_by_resources() {
        let conf = val!({
            "task": {
                "worker-memory-mb": 2048i64,
                "code-vm-memory-mb": 256i64,
                "container-reserved-memory-mb": 512i64,
                "recovery-reserved-slots": 1i64,
            },
            "worker": {
                "shared-process": false,
            },
        });

        let budget = derive_task_code_concurrency(&conf, 500);

        assert!(budget.resolved >= 1);
        assert!(budget.resolved <= 500);
    }

    #[test]
    fn postgres_pool_includes_execution_capacity_and_reserved_connections() {
        let conf = val!({
            "worker": {
                "threads": 4i64,
                "vm-concurrency": 3i64,
                "db-reserved-connections": 4i64,
                "shared-process": false,
            },
        });

        let max_connections = derive_postgres_pool_connections(&conf);

        assert_eq!(max_connections, 10);
    }

    #[test]
    fn sqlite_pool_keeps_read_floor_and_adds_write_headroom() {
        let conf = val!({
            "worker": {
                "threads": 4i64,
                "local-write-concurrency": 2i64,
            },
        });

        let max_connections = derive_sqlite_pool_connections(&conf);

        // Pool stays at least at the historical floor so reads never serialize
        // behind a single connection.
        assert!(max_connections >= 10);
    }

    #[test]
    fn postgres_pool_respects_explicit_ceiling() {
        let conf = val!({
            "worker": {
                "threads": 64i64,
                "db-max-connections": 20i64,
                "db-reserved-connections": 4i64,
            },
        });

        let max_connections = derive_postgres_pool_connections(&conf);

        assert!(max_connections >= 10);
        assert!(max_connections <= 20);
    }

    #[test]
    fn postgres_pool_respects_explicit_ceiling_below_default_floor() {
        let conf = val!({
            "worker": {
                "threads": 64i64,
                "db-max-connections": 5i64,
                "db-reserved-connections": 4i64,
            },
        });

        let max_connections = derive_postgres_pool_connections(&conf);

        assert_eq!(max_connections, 5);
    }

    #[test]
    fn scheduler_singleton_keeps_connection_for_work_beside_advisory_lock() {
        let conf = val!({
            "scheduler": {
                "singleton": true,
            },
            "worker": {
                "db-max-connections": 1i64,
            },
        });

        let max_connections = derive_postgres_pool_connections(&conf);

        assert_eq!(max_connections, 2);
    }

    #[test]
    fn postgres_pool_ignores_worker_budget_when_threads_are_not_configured() {
        let conf = val!({
            "task": {
                "code-max-concurrent": 2i64,
                "worker-memory-mb": 2048i64,
                "code-vm-memory-mb": 256i64,
            },
            "worker": {
                "db-reserved-connections": 4i64,
            },
        });

        let max_connections = derive_postgres_pool_connections(&conf);

        assert_eq!(max_connections, 10);
    }

    #[test]
    fn task_container_concurrency_derives_from_memory_disk_and_recovery_reserve() {
        let conf = val!({
            "task": {
                "container-max-concurrent": "auto",
                "container-reserved-memory-mb": 512i64,
                "container-reserved-disk-mb": 10_240i64,
                "recovery-reserved-slots": 1i64,
            },
        });

        let budget =
            derive_task_container_concurrency(&conf, 8, 4096, 30_720, 512, 5_120, 500, "docker");
        // ceiling - process reserve (512) - recovery reserve (512) -> usable,
        // then containers take the 50% complement of the code-VM share.
        let ceiling = detected_memory_limit_mb()
            .map(|limit| limit.min(4096))
            .unwrap_or(4096);
        let usable = ceiling.saturating_sub(512).saturating_sub(512).max(1);
        let expected_memory_budget = (usable * 50 / 100).max(1);
        let expected_memory_limit = (expected_memory_budget / 1012).max(1) as usize;

        assert_eq!(budget.requested, 8);
        assert_eq!(budget.memory_budget_mb, expected_memory_budget);
        assert_eq!(budget.disk_budget_mb, 20_480);
        assert_eq!(budget.memory_limit, expected_memory_limit);
        assert_eq!(budget.disk_limit, 4);
        assert_eq!(budget.resolved, 8.min(expected_memory_limit).min(4));
        assert!(!budget.explicit);
    }

    #[test]
    fn task_container_concurrency_explicit_limit_still_respects_resources() {
        let conf = val!({
            "task": {
                "container-max-concurrent": 10i64,
                "container-reserved-memory-mb": 512i64,
                "container-reserved-disk-mb": 10_240i64,
                "recovery-reserved-slots": 1i64,
            },
        });

        let budget =
            derive_task_container_concurrency(&conf, 4, 2048, 20_480, 512, 5_120, 500, "kata");

        assert_eq!(budget.requested, 10);
        assert!(budget.resolved <= 10);
        assert!(budget.explicit);
        assert_eq!(budget.backend, "kata");
    }
}

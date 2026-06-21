use crate::val::Val;
use std::num::NonZeroUsize;

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

    if let Some(explicit) = configured_usize(conf, "worker.vm-concurrency") {
        return DerivedConcurrency {
            requested,
            resolved: explicit.min(requested).max(1),
            cpu_limit: requested,
            memory_limit: None,
            memory_limit_mb: detected_memory_limit_mb(),
            explicit: true,
            shared_process,
        };
    }

    let cpu_limit = effective_cpu_parallelism(shared_process).max(1);
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
    let memory_limit_mb = detected_memory_limit_mb().or_else(|| {
        let configured = conf.get_int_or_default("task.worker-memory-mb", 8192);
        (configured > 0).then_some(configured as u64)
    });
    let vm_memory_mb = conf
        .get_int_or_default("task.code-vm-memory-mb", 256)
        .max(1) as u64;
    let container_reserve_mb = conf
        .get_int_or_default("task.container-reserved-memory-mb", 512)
        .max(0) as u64;
    let recovery_slots = conf
        .get_int_or_default("task.recovery-reserved-slots", 1)
        .max(0) as u64;
    let process_reserve_mb = 512;
    let reserved_memory_mb =
        process_reserve_mb + container_reserve_mb.saturating_mul(recovery_slots.max(1));
    let memory_limit = memory_limit_mb.map(|limit| {
        limit
            .saturating_sub(reserved_memory_mb)
            .checked_div(vm_memory_mb)
            .unwrap_or(1)
            .max(1) as usize
    });

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
    let reserved_memory_mb = conf
        .get_int_or_default("task.container-reserved-memory-mb", 512)
        .max(0) as u64
        * recovery_reserved_slots.max(1) as u64;
    let reserved_disk_mb = conf
        .get_int_or_default("task.container-reserved-disk-mb", 10_240)
        .max(0) as u64
        * recovery_reserved_slots.max(1) as u64;

    let detected_memory_mb = detected_memory_limit_mb();
    let memory_ceiling_mb = detected_memory_mb
        .map(|limit| limit.min(worker_memory_mb.max(1)))
        .unwrap_or_else(|| worker_memory_mb.max(1));
    let memory_budget_mb = memory_ceiling_mb.saturating_sub(reserved_memory_mb).max(1);
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
    let worker_budget = if conf.get("worker").is_some() {
        let requested = conf.get_int_or_default("worker.threads", 4).max(1) as usize;
        derive_worker_vm_concurrency(conf, requested).resolved
    } else {
        0
    };
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

    capacity_budget
        .saturating_add(reserved)
        .max(10)
        .min(u32::MAX as usize) as u32
}

pub fn derive_sqlite_pool_connections(conf: &Val) -> u32 {
    conf.get_int_or_default("worker.local-write-concurrency", 1)
        .max(1)
        .min(u32::MAX as i64) as u32
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
    fn sqlite_pool_uses_local_write_concurrency_cap() {
        let conf = val!({
            "worker": {
                "local-write-concurrency": 2i64,
            },
        });

        let max_connections = derive_sqlite_pool_connections(&conf);

        assert_eq!(max_connections, 2);
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
        let expected_memory_budget = detected_memory_limit_mb()
            .map(|limit| limit.min(4096))
            .unwrap_or(4096)
            .saturating_sub(512)
            .max(1);
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

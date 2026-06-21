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
}

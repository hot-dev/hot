//! Resource-aware admission control for container tasks.
//!
//! Instead of a fixed semaphore, this tracks memory and disk budgets.
//! A container must `acquire()` resources before starting; if insufficient
//! budget is available, the call waits (with timeout) until another container
//! releases its resources.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Notify;

#[derive(Debug)]
pub struct ResourceBudget {
    total_memory_mb: u64,
    total_disk_mb: u64,
    allocated_memory_mb: AtomicU64,
    allocated_disk_mb: AtomicU64,
    notify: Notify,
}

impl ResourceBudget {
    pub fn new(total_memory_mb: u64, total_disk_mb: u64) -> Arc<Self> {
        Arc::new(Self {
            total_memory_mb,
            total_disk_mb,
            allocated_memory_mb: AtomicU64::new(0),
            allocated_disk_mb: AtomicU64::new(0),
            notify: Notify::new(),
        })
    }

    /// Try to acquire resources. Returns a guard that releases on drop.
    /// Waits up to `timeout` for resources to become available.
    pub async fn acquire(
        self: &Arc<Self>,
        memory_mb: u64,
        disk_mb: u64,
        timeout: std::time::Duration,
    ) -> Result<ResourceGuard, ResourceBudgetError> {
        if memory_mb > self.total_memory_mb || disk_mb > self.total_disk_mb {
            return Err(ResourceBudgetError::InsufficientCapacity {
                requested_memory_mb: memory_mb,
                requested_disk_mb: disk_mb,
                total_memory_mb: self.total_memory_mb,
                total_disk_mb: self.total_disk_mb,
            });
        }

        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let cur_mem = self.allocated_memory_mb.load(Ordering::Acquire);
            let cur_disk = self.allocated_disk_mb.load(Ordering::Acquire);

            if cur_mem + memory_mb <= self.total_memory_mb
                && cur_disk + disk_mb <= self.total_disk_mb
            {
                // Try CAS for memory
                if self
                    .allocated_memory_mb
                    .compare_exchange_weak(
                        cur_mem,
                        cur_mem + memory_mb,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    // Try CAS for disk
                    if self
                        .allocated_disk_mb
                        .compare_exchange_weak(
                            cur_disk,
                            cur_disk + disk_mb,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return Ok(ResourceGuard {
                            budget: Arc::clone(self),
                            memory_mb,
                            disk_mb,
                        });
                    }
                    // Disk CAS failed, rollback memory
                    self.allocated_memory_mb
                        .fetch_sub(memory_mb, Ordering::AcqRel);
                }
                // CAS failed, retry immediately
                continue;
            }

            // Not enough resources — wait for notification or timeout
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(ResourceBudgetError::Timeout {
                    requested_memory_mb: memory_mb,
                    requested_disk_mb: disk_mb,
                    available_memory_mb: self.total_memory_mb.saturating_sub(cur_mem),
                    available_disk_mb: self.total_disk_mb.saturating_sub(cur_disk),
                });
            }

            tokio::time::timeout(remaining, self.notify.notified())
                .await
                .ok();
        }
    }

    pub fn available_memory_mb(&self) -> u64 {
        self.total_memory_mb
            .saturating_sub(self.allocated_memory_mb.load(Ordering::Acquire))
    }

    pub fn available_disk_mb(&self) -> u64 {
        self.total_disk_mb
            .saturating_sub(self.allocated_disk_mb.load(Ordering::Acquire))
    }

    fn release(&self, memory_mb: u64, disk_mb: u64) {
        self.allocated_memory_mb
            .fetch_sub(memory_mb, Ordering::AcqRel);
        self.allocated_disk_mb.fetch_sub(disk_mb, Ordering::AcqRel);
        self.notify.notify_waiters();
    }
}

pub struct ResourceGuard {
    budget: Arc<ResourceBudget>,
    memory_mb: u64,
    disk_mb: u64,
}

impl Drop for ResourceGuard {
    fn drop(&mut self) {
        self.budget.release(self.memory_mb, self.disk_mb);
    }
}

#[derive(Debug)]
pub enum ResourceBudgetError {
    InsufficientCapacity {
        requested_memory_mb: u64,
        requested_disk_mb: u64,
        total_memory_mb: u64,
        total_disk_mb: u64,
    },
    Timeout {
        requested_memory_mb: u64,
        requested_disk_mb: u64,
        available_memory_mb: u64,
        available_disk_mb: u64,
    },
}

impl std::fmt::Display for ResourceBudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientCapacity {
                requested_memory_mb,
                requested_disk_mb,
                total_memory_mb,
                total_disk_mb,
            } => write!(
                f,
                "Requested resources exceed worker capacity: requested {}MB memory + {}MB disk, \
                 capacity {}MB memory + {}MB disk",
                requested_memory_mb, requested_disk_mb, total_memory_mb, total_disk_mb,
            ),
            Self::Timeout {
                requested_memory_mb,
                requested_disk_mb,
                available_memory_mb,
                available_disk_mb,
            } => write!(
                f,
                "Timed out waiting for resources: requested {}MB memory + {}MB disk, \
                 available {}MB memory + {}MB disk",
                requested_memory_mb, requested_disk_mb, available_memory_mb, available_disk_mb,
            ),
        }
    }
}

impl std::error::Error for ResourceBudgetError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_acquire_and_release() {
        let budget = ResourceBudget::new(1024, 10240);
        let guard = budget
            .acquire(512, 5120, std::time::Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(budget.available_memory_mb(), 512);
        assert_eq!(budget.available_disk_mb(), 5120);

        drop(guard);

        assert_eq!(budget.available_memory_mb(), 1024);
        assert_eq!(budget.available_disk_mb(), 10240);
    }

    #[tokio::test]
    async fn test_acquire_multiple() {
        let budget = ResourceBudget::new(2048, 20480);
        let g1 = budget
            .acquire(512, 5120, std::time::Duration::from_secs(1))
            .await
            .unwrap();
        let g2 = budget
            .acquire(1024, 10240, std::time::Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(budget.available_memory_mb(), 512);
        assert_eq!(budget.available_disk_mb(), 5120);

        drop(g1);
        assert_eq!(budget.available_memory_mb(), 1024);

        drop(g2);
        assert_eq!(budget.available_memory_mb(), 2048);
    }

    #[tokio::test]
    async fn test_acquire_timeout_insufficient() {
        let budget = ResourceBudget::new(512, 5120);
        let _g = budget
            .acquire(512, 5120, std::time::Duration::from_secs(1))
            .await
            .unwrap();

        let result = budget
            .acquire(256, 1024, std::time::Duration::from_millis(50))
            .await;
        assert!(matches!(result, Err(ResourceBudgetError::Timeout { .. })));
    }

    #[tokio::test]
    async fn test_acquire_rejects_request_larger_than_capacity() {
        let budget = ResourceBudget::new(512, 5120);

        let result = budget
            .acquire(768, 1024, std::time::Duration::from_millis(50))
            .await;
        assert!(matches!(
            result,
            Err(ResourceBudgetError::InsufficientCapacity { .. })
        ));
    }

    #[tokio::test]
    async fn test_acquire_waits_for_release() {
        let budget = ResourceBudget::new(512, 5120);
        let guard = budget
            .acquire(512, 5120, std::time::Duration::from_secs(1))
            .await
            .unwrap();

        let budget_clone = Arc::clone(&budget);
        let handle = tokio::spawn(async move {
            budget_clone
                .acquire(256, 1024, std::time::Duration::from_secs(2))
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(guard);

        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }
}

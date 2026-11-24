//! Resource-aware admission control for container tasks.
//!
//! Instead of a fixed semaphore, this tracks memory and disk budgets.
//! A container must `acquire()` resources before starting; if insufficient
//! budget is available, the call waits (with timeout) until another container
//! releases its resources.

use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::Notify;

/// Account for writable snapshot/rootfs metadata and image unpacking in
/// addition to the user-visible `/data` allocation. The proportional term
/// avoids admitting very large writable roots against a data-only budget.
pub fn disk_admission_mb(data_mb: u64) -> u64 {
    data_mb.saturating_add(pool_admission_mb(data_mb))
}

/// What a box takes from Kata's box storage pool: its image and writable
/// rootfs, which `disk_admission_mb` counts on top of `/data`. `/data` is a
/// file of its own outside the pool (see `data_volume`), which the disk it is
/// on is checked for.
#[cfg_attr(not(all(target_os = "linux", feature = "kata")), allow(dead_code))]
pub fn pool_admission_mb(data_mb: u64) -> u64 {
    const BASE_IMAGE_AND_SNAPSHOT_OVERHEAD_MB: u64 = 2_048;
    BASE_IMAGE_AND_SNAPSHOT_OVERHEAD_MB.saturating_add(data_mb / 4)
}

#[derive(Debug, Default)]
struct Allocated {
    memory_mb: u64,
    disk_mb: u64,
}

#[derive(Debug)]
pub struct ResourceBudget {
    total_memory_mb: u64,
    total_disk_mb: u64,
    allocated: Mutex<Allocated>,
    notify: Notify,
}

impl ResourceBudget {
    pub fn new(total_memory_mb: u64, total_disk_mb: u64) -> Arc<Self> {
        Arc::new(Self {
            total_memory_mb,
            total_disk_mb,
            allocated: Mutex::new(Allocated::default()),
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
            // Pin and enable the waiter *before* locking `allocated` so a
            // concurrent `release()` cannot fire `notify_waiters()` in the gap
            // between the check and `.await` and cause a lost wakeup.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let (cur_mem, cur_disk) = {
                let mut allocated = self.allocated.lock();
                if allocated.memory_mb.saturating_add(memory_mb) <= self.total_memory_mb
                    && allocated.disk_mb.saturating_add(disk_mb) <= self.total_disk_mb
                {
                    allocated.memory_mb += memory_mb;
                    allocated.disk_mb += disk_mb;
                    return Ok(ResourceGuard {
                        budget: Arc::clone(self),
                        memory_mb,
                        disk_mb,
                    });
                }
                (allocated.memory_mb, allocated.disk_mb)
            };

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

            tokio::time::timeout(remaining, notified).await.ok();
        }
    }

    pub fn total_memory_mb(&self) -> u64 {
        self.total_memory_mb
    }

    pub fn available_memory_mb(&self) -> u64 {
        let allocated = self.allocated.lock();
        self.total_memory_mb.saturating_sub(allocated.memory_mb)
    }

    pub fn available_disk_mb(&self) -> u64 {
        let allocated = self.allocated.lock();
        self.total_disk_mb.saturating_sub(allocated.disk_mb)
    }

    fn release(&self, memory_mb: u64, disk_mb: u64) {
        {
            let mut allocated = self.allocated.lock();
            allocated.memory_mb = allocated.memory_mb.saturating_sub(memory_mb);
            allocated.disk_mb = allocated.disk_mb.saturating_sub(disk_mb);
        }
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

    #[test]
    fn disk_admission_includes_rootfs_overhead() {
        assert_eq!(disk_admission_mb(0), 2_048);
        assert_eq!(disk_admission_mb(4_096), 7_168);
        assert!(disk_admission_mb(u64::MAX).eq(&u64::MAX));
    }

    #[test]
    fn the_box_pool_holds_the_rootfs_not_data() {
        assert_eq!(pool_admission_mb(0), 2_048);
        assert_eq!(pool_admission_mb(10_240), 4_608);
        assert_eq!(
            disk_admission_mb(10_240),
            10_240 + pool_admission_mb(10_240)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_acquire_and_release_does_not_lose_wakeups() {
        // Stress the check-then-wait race across OS threads: if notified()
        // were not enabled before loading the atomics, releases between the
        // atomic load and await would lose wakeups and hit the tight timeout.
        let budget = ResourceBudget::new(512, 5120);
        for _ in 0..200 {
            let guard = budget
                .acquire(512, 5120, std::time::Duration::from_secs(1))
                .await
                .unwrap();
            let b = Arc::clone(&budget);
            let waiter = tokio::spawn(async move {
                b.acquire(512, 5120, std::time::Duration::from_millis(500))
                    .await
            });
            // Release immediately on another thread without sleeping so release
            // races directly against waiter's check-and-park path.
            tokio::task::yield_now().await;
            drop(guard);
            let acquired = waiter
                .await
                .unwrap()
                .expect("waiter must not lose wakeup on concurrent release");
            drop(acquired);
        }
        assert_eq!(budget.available_memory_mb(), 512);
        assert_eq!(budget.available_disk_mb(), 5120);
    }
}

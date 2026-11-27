//! In-process wakeup hub for `::hot::task/await`.
//!
//! Coordinates per-`task_id` `tokio::sync::Notify` waiters with RAII cleanup
//! on drop so multiple concurrent awaits on the same pod share one underlying
//! pub/sub transport (one Redis Streams reader in distributed mode, or direct
//! in-process notifications in memory mode) without leaking waiter entries.

use ahash::AHashMap;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use uuid::Uuid;

/// Maximum number of recently completed task IDs retained so a waiter that
/// registers slightly after a completion notification (for example right after
/// a retry-lineage DB lookup) still wakes immediately instead of waiting for
/// the first polling backoff tick.
const RECENT_COMPLETIONS_CAP: usize = 1024;
const RECENT_COMPLETIONS_TTL: Duration = Duration::from_secs(30);

struct HubState {
    next_waiter_id: u64,
    waiters: AHashMap<Uuid, Vec<(u64, Arc<Notify>)>>,
    active_waiter_count: usize,
    recent_order: VecDeque<(Uuid, Instant)>,
    recent: AHashMap<Uuid, Instant>,
}

impl HubState {
    fn new() -> Self {
        Self {
            next_waiter_id: 0,
            waiters: AHashMap::new(),
            active_waiter_count: 0,
            recent_order: VecDeque::new(),
            recent: AHashMap::new(),
        }
    }

    fn prune_recent(&mut self, now: Instant) {
        while let Some(&(id, recorded_at)) = self.recent_order.front() {
            let expired = now.saturating_duration_since(recorded_at) > RECENT_COMPLETIONS_TTL;
            let over_cap = self.recent_order.len() > RECENT_COMPLETIONS_CAP;
            if !expired && !over_cap {
                break;
            }
            self.recent_order.pop_front();
            // Only remove from `recent` if no newer entry for the same id superseded it.
            if self.recent.get(&id).copied() == Some(recorded_at) {
                self.recent.remove(&id);
            }
        }
    }
}

/// Shared per-`StreamPubSub` registry mapping awaited `task_id`s to waiter
/// notifiers.
pub(crate) struct TaskWakeupHub {
    state: Mutex<HubState>,
}

impl TaskWakeupHub {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(HubState::new()),
        }
    }

    /// Register a new waiter for `task_id`.
    ///
    /// Each guard owns an exclusive `Arc<Notify>` woken via `notify_one()`, so
    /// notifications store a single permit and cannot be lost between
    /// registration and entering `guard.notified().await`.
    pub(crate) fn register(self: &Arc<Self>, task_id: Uuid) -> TaskWakeupGuard {
        let notify = Arc::new(Notify::new());
        let waiter_id =
            {
                let mut state = self.state.lock();
                let now = Instant::now();
                state.prune_recent(now);

                let id = state.next_waiter_id;
                state.next_waiter_id = state.next_waiter_id.wrapping_add(1);

                if state.recent.get(&task_id).is_some_and(|seen| {
                    now.saturating_duration_since(*seen) <= RECENT_COMPLETIONS_TTL
                }) {
                    notify.notify_one();
                }

                state
                    .waiters
                    .entry(task_id)
                    .or_default()
                    .push((id, Arc::clone(&notify)));
                state.active_waiter_count += 1;
                id
            };

        TaskWakeupGuard {
            task_id,
            waiter_id,
            notify,
            hub: Arc::clone(self),
        }
    }

    /// Deliver a best-effort wakeup hint to every waiter awaiting `task_id`.
    pub(crate) fn notify(&self, task_id: Uuid) {
        let targets: Vec<Arc<Notify>> = {
            let mut state = self.state.lock();
            let now = Instant::now();
            state.recent.insert(task_id, now);
            state.recent_order.push_back((task_id, now));
            state.prune_recent(now);

            state
                .waiters
                .get(&task_id)
                .map(|entries| entries.iter().map(|(_, n)| Arc::clone(n)).collect())
                .unwrap_or_default()
        };

        for notify in targets {
            notify.notify_one();
        }
    }

    /// Wake every currently registered waiter once (for example after the
    /// shared Redis reader reconnects following an outage during which stream
    /// entries may have been trimmed).
    pub(crate) fn notify_all(&self) {
        let targets: Vec<Arc<Notify>> = {
            let state = self.state.lock();
            state
                .waiters
                .values()
                .flat_map(|entries| entries.iter().map(|(_, n)| Arc::clone(n)))
                .collect()
        };

        for notify in targets {
            notify.notify_one();
        }
    }

    fn unregister(&self, task_id: Uuid, waiter_id: u64) {
        let mut state = self.state.lock();
        let mut removed = false;
        let mut empty = false;
        if let Some(entries) = state.waiters.get_mut(&task_id)
            && let Some(pos) = entries.iter().position(|(id, _)| *id == waiter_id)
        {
            entries.swap_remove(pos);
            removed = true;
            empty = entries.is_empty();
        }
        if empty {
            state.waiters.remove(&task_id);
        }
        if removed {
            state.active_waiter_count = state.active_waiter_count.saturating_sub(1);
        }
    }

    pub(crate) fn active_waiter_count(&self) -> usize {
        self.state.lock().active_waiter_count
    }

    #[cfg(test)]
    pub(crate) fn tracked_task_count(&self) -> usize {
        self.state.lock().waiters.len()
    }
}

/// RAII registration for a single `::hot::task/await` wakeup waiter.
///
/// Dropping the guard immediately unregisters the waiter from its hub so
/// long-lived workers never leak entries.
pub struct TaskWakeupGuard {
    task_id: Uuid,
    waiter_id: u64,
    notify: Arc<Notify>,
    hub: Arc<TaskWakeupHub>,
}

impl TaskWakeupGuard {
    /// Wait for the next completion wakeup hint for this task.
    ///
    /// Cancel-safe: each guard owns an exclusive `Notify` woken via
    /// `notify_one()`, so a hint that arrives before `notified().await` is
    /// polled is preserved as a stored permit.
    pub async fn notified(&self) {
        self.notify.notified().await;
    }

    pub fn task_id(&self) -> Uuid {
        self.task_id
    }
}

impl Drop for TaskWakeupGuard {
    fn drop(&mut self) {
        self.hub.unregister(self.task_id, self.waiter_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn multiple_waiters_on_same_and_distinct_tasks_clean_up_on_drop() {
        let hub = Arc::new(TaskWakeupHub::new());
        let task_a = Uuid::now_v7();
        let task_b = Uuid::now_v7();

        let guard_a1 = hub.register(task_a);
        let guard_a2 = hub.register(task_a);
        let guard_b = hub.register(task_b);
        assert_eq!(hub.active_waiter_count(), 3);
        assert_eq!(hub.tracked_task_count(), 2);

        hub.notify(task_a);
        tokio::time::timeout(Duration::from_millis(50), guard_a1.notified())
            .await
            .expect("first waiter for task_a must wake");
        tokio::time::timeout(Duration::from_millis(50), guard_a2.notified())
            .await
            .expect("second waiter for task_a must wake");
        assert!(
            tokio::time::timeout(Duration::from_millis(20), guard_b.notified())
                .await
                .is_err(),
            "unrelated task_b waiter must not wake for task_a"
        );

        drop(guard_a1);
        assert_eq!(hub.active_waiter_count(), 2);
        assert_eq!(hub.tracked_task_count(), 2);

        drop(guard_a2);
        assert_eq!(hub.active_waiter_count(), 1);
        assert_eq!(hub.tracked_task_count(), 1);

        drop(guard_b);
        assert_eq!(hub.active_waiter_count(), 0);
        assert_eq!(hub.tracked_task_count(), 0);
    }

    #[tokio::test]
    async fn notification_before_await_or_registration_is_not_lost() {
        let hub = Arc::new(TaskWakeupHub::new());

        // Case 1: notify arrives after register() but before notified().await
        let task_a = Uuid::now_v7();
        let guard_a = hub.register(task_a);
        hub.notify(task_a);
        tokio::time::timeout(Duration::from_millis(50), guard_a.notified())
            .await
            .expect("stored permit must wake notified() immediately");

        // Case 2: notify arrives slightly before register() (e.g. retry hop)
        let task_b = Uuid::now_v7();
        hub.notify(task_b);
        let guard_b = hub.register(task_b);
        tokio::time::timeout(Duration::from_millis(50), guard_b.notified())
            .await
            .expect("recent completion cache must wake late registration immediately");
    }

    #[tokio::test]
    async fn notify_all_wakes_all_active_waiters() {
        let hub = Arc::new(TaskWakeupHub::new());
        let g1 = hub.register(Uuid::now_v7());
        let g2 = hub.register(Uuid::now_v7());

        hub.notify_all();
        tokio::time::timeout(Duration::from_millis(50), g1.notified())
            .await
            .expect("g1 must wake");
        tokio::time::timeout(Duration::from_millis(50), g2.notified())
            .await
            .expect("g2 must wake");
    }
}

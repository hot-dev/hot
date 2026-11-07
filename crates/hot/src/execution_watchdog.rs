//! Track blocking executions after their async owner leaves. Cancellation gets
//! a grace period; a process whose blocking pool is wedged drains and restarts.
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;
use uuid::Uuid;

pub struct Watchdog {
    limit: AtomicUsize,
    detached: AtomicUsize,
    restart: AtomicBool,
    wake: Notify,
}
impl Watchdog {
    fn new() -> Self {
        Self {
            limit: AtomicUsize::new(1),
            detached: AtomicUsize::new(0),
            restart: AtomicBool::new(false),
            wake: Notify::new(),
        }
    }
    pub fn configure(&self, limit: usize) {
        self.limit.store(limit.max(1), Ordering::Release);
    }
    pub async fn restart_requested(&self) {
        loop {
            let notified = self.wake.notified();
            if self.restart.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
    pub fn execution(
        self: &Arc<Self>,
        org: Option<Uuid>,
        id: Uuid,
    ) -> (CompletionGuard, ExecutionWatch) {
        let state = Arc::new(State {
            complete: AtomicBool::new(false),
            counted: std::sync::Mutex::new(false),
            watchdog: self.clone(),
            org,
            id,
        });
        (CompletionGuard(state.clone()), ExecutionWatch(state))
    }
}
struct State {
    complete: AtomicBool,
    counted: std::sync::Mutex<bool>,
    watchdog: Arc<Watchdog>,
    org: Option<Uuid>,
    id: Uuid,
}
impl State {
    fn count_if_running(&self) {
        let mut counted = self.counted.lock().unwrap_or_else(|e| e.into_inner());
        if self.complete.load(Ordering::Acquire) || *counted {
            return;
        }
        *counted = true;
        let count = self.watchdog.detached.fetch_add(1, Ordering::AcqRel) + 1;
        tracing::error!(org_id = ?self.org, execution_id = %self.id, detached = count, "Blocking execution ignored cancellation after grace period");
        if count >= self.watchdog.limit.load(Ordering::Acquire) {
            self.watchdog.restart.store(true, Ordering::Release);
            self.watchdog.wake.notify_one();
        }
    }
}
pub struct CompletionGuard(Arc<State>);
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        let counted = self.0.counted.lock().unwrap_or_else(|e| e.into_inner());
        self.0.complete.store(true, Ordering::Release);
        if *counted {
            self.0.watchdog.detached.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
pub struct ExecutionWatch(Arc<State>);
impl Drop for ExecutionWatch {
    fn drop(&mut self) {
        if self.0.complete.load(Ordering::Acquire) {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let state = self.0.clone();
            runtime.spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                state.count_if_running();
            });
        }
    }
}
pub fn events() -> &'static Arc<Watchdog> {
    static INSTANCE: OnceLock<Arc<Watchdog>> = OnceLock::new();
    INSTANCE.get_or_init(|| Arc::new(Watchdog::new()))
}
pub fn tasks() -> &'static Arc<Watchdog> {
    static INSTANCE: OnceLock<Arc<Watchdog>> = OnceLock::new();
    INSTANCE.get_or_init(|| Arc::new(Watchdog::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn requests_restart_at_ceiling_and_releases_finished_threads() {
        let watchdog = Arc::new(Watchdog::new());
        watchdog.configure(2);
        let (one, watch_one) = watchdog.execution(Some(Uuid::now_v7()), Uuid::now_v7());
        let (two, watch_two) = watchdog.execution(None, Uuid::now_v7());
        watch_one.0.count_if_running();
        assert!(!watchdog.restart.load(Ordering::Acquire));
        watch_two.0.count_if_running();
        watchdog.restart_requested().await;
        assert_eq!(watchdog.detached.load(Ordering::Acquire), 2);
        drop(one);
        drop(two);
        assert_eq!(watchdog.detached.load(Ordering::Acquire), 0);
    }
    #[test]
    fn completed_execution_is_never_counted() {
        let watchdog = Arc::new(Watchdog::new());
        let (guard, watch) = watchdog.execution(None, Uuid::now_v7());
        drop(guard);
        watch.0.count_if_running();
        assert_eq!(watchdog.detached.load(Ordering::Acquire), 0);
    }
}

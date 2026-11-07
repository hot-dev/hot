//! How many event runs execute at once, not counting runs that only wait.
//!
//! A run spends most of its life waiting on tasks or remote services, which
//! costs a VM thread but no CPU. Sizing the worker by CPU count alone lets a
//! few such runs hold every executor while short runs queue behind them.
//!
//! So the worker has two limits. Its executors bound how many runs it holds at
//! all, which is a memory limit. [`RunSlots`] bounds how many of them execute,
//! which is a CPU limit. A run holds a slot only for its VM: setup and the
//! writes after it are database waits, and need none. It gives the slot up
//! while it waits ([`RunSlotHandle::park`]) and takes one back to continue.
//!
//! A run that has started takes a slot before any run that hasn't, so a run
//! pays for a wait at most once, not once behind every queued run.
//!
//! Slots are not a hard cap. A `parallel` flow runs several VMs under one slot,
//! as it always has, and the slot is given up when the first of them waits.

use parking_lot::Mutex;
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

/// How often a run waiting to continue checks whether it was cancelled.
const RESUME_CANCEL_POLL: Duration = Duration::from_millis(250);

/// The slots of one worker process.
#[derive(Debug, Clone)]
pub struct RunSlots {
    pool: Arc<Pool>,
}

#[derive(Debug)]
struct Pool {
    semaphore: Arc<Semaphore>,
    /// How many started runs are waiting to take a slot back.
    resuming: watch::Sender<usize>,
}

impl RunSlots {
    pub fn new(limit: usize) -> Self {
        Self {
            pool: Arc::new(Pool {
                semaphore: Arc::new(Semaphore::new(limit.clamp(1, Semaphore::MAX_PERMITS))),
                resuming: watch::Sender::new(0),
            }),
        }
    }

    /// Wait for a slot for a run that hasn't started. Cancel-safe.
    pub async fn acquire(&self) -> RunSlot {
        let mut resuming = self.pool.resuming.subscribe();
        loop {
            let _ = resuming.wait_for(|count| *count == 0).await;
            let permit = Arc::clone(&self.pool.semaphore).acquire_owned().await.ok();
            // A run resumed while this one queued: the slot goes to it.
            if *resuming.borrow() == 0 {
                return RunSlot {
                    handle: RunSlotHandle {
                        shared: Arc::new(Shared {
                            pool: Arc::clone(&self.pool),
                            state: Mutex::new(SlotState {
                                permit,
                                waiting: 0,
                                closed: false,
                            }),
                        }),
                    },
                };
            }
        }
    }

    /// Slots free right now.
    pub fn available(&self) -> usize {
        self.pool.semaphore.available_permits()
    }
}

#[derive(Debug)]
struct Shared {
    pool: Arc<Pool>,
    state: Mutex<SlotState>,
}

#[derive(Debug)]
struct SlotState {
    permit: Option<OwnedSemaphorePermit>,
    /// How many of the run's VMs are waiting.
    waiting: usize,
    /// The executor is done with the run. A timed-out VM can outlive it, and
    /// must not take a slot for work nobody is waiting on.
    closed: bool,
}

/// A slot held for one run. Dropping it gives the slot back, whatever the
/// run's VMs are doing.
#[derive(Debug)]
pub struct RunSlot {
    handle: RunSlotHandle,
}

impl RunSlot {
    pub fn handle(&self) -> RunSlotHandle {
        self.handle.clone()
    }
}

impl Drop for RunSlot {
    fn drop(&mut self) {
        let mut state = self.handle.shared.state.lock();
        state.closed = true;
        state.permit = None;
    }
}

/// What a run's VMs hold to give their slot up while they wait.
#[derive(Debug, Clone)]
pub struct RunSlotHandle {
    shared: Arc<Shared>,
}

impl RunSlotHandle {
    fn release(&self) {
        let mut state = self.shared.state.lock();
        state.waiting += 1;
        state.permit = None;
    }

    /// Whether this was the run's last wait, and the run needs its slot back.
    fn finish_wait(&self) -> bool {
        let mut state = self.shared.state.lock();
        state.waiting = state.waiting.saturating_sub(1);
        state.waiting == 0 && !state.closed && state.permit.is_none()
    }

    fn take_back(&self, permit: OwnedSemaphorePermit) {
        let mut state = self.shared.state.lock();
        if state.waiting == 0 && !state.closed && state.permit.is_none() {
            state.permit = Some(permit);
        }
    }

    /// Give the slot up until the returned guard drops. Dropping it blocks
    /// until a slot is free, so call this only from a thread that may block,
    /// as a VM's is. A cancelled run continues without a slot: it is stopping,
    /// and must not queue to do so.
    pub fn park(&self, cancel: Option<Arc<AtomicBool>>) -> ParkedRun {
        self.release();
        ParkedRun {
            handle: self.clone(),
            cancel,
        }
    }
}

/// Held while a run waits. See [`RunSlotHandle::park`].
#[derive(Debug)]
pub struct ParkedRun {
    handle: RunSlotHandle,
    cancel: Option<Arc<AtomicBool>>,
}

impl Drop for ParkedRun {
    fn drop(&mut self) {
        if !self.handle.finish_wait() {
            return;
        }
        let pool = &self.handle.shared.pool;
        let permit = match Arc::clone(&pool.semaphore).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => tokio::runtime::Handle::try_current()
                .ok()
                .and_then(|runtime| {
                    pool.resuming.send_modify(|count| *count += 1);
                    let permit = runtime.block_on(acquire_unless_cancelled(
                        Arc::clone(&pool.semaphore),
                        self.cancel.take(),
                    ));
                    pool.resuming.send_modify(|count| *count -= 1);
                    permit
                }),
        };
        if let Some(permit) = permit {
            self.handle.take_back(permit);
        }
    }
}

async fn acquire_unless_cancelled(
    semaphore: Arc<Semaphore>,
    cancel: Option<Arc<AtomicBool>>,
) -> Option<OwnedSemaphorePermit> {
    let Some(cancel) = cancel else {
        return semaphore.acquire_owned().await.ok();
    };
    let mut acquire = std::pin::pin!(semaphore.acquire_owned());
    loop {
        tokio::select! {
            biased;
            permit = &mut acquire => return permit.ok(),
            _ = tokio::time::sleep(RESUME_CANCEL_POLL) => {
                if cancel.load(Ordering::Relaxed) {
                    return None;
                }
            }
        }
    }
}

thread_local! {
    static CURRENT_RUN_SLOT: RefCell<Option<RunSlotHandle>> = const { RefCell::new(None) };
}

/// Makes a run's slot the one VMs created on this thread use, until dropped.
#[derive(Debug)]
pub struct RunSlotScope {
    previous: Option<RunSlotHandle>,
    _not_send: std::marker::PhantomData<*const ()>,
}

pub fn enter_run_slot(handle: Option<RunSlotHandle>) -> RunSlotScope {
    RunSlotScope {
        previous: CURRENT_RUN_SLOT.with(|current| current.replace(handle)),
        _not_send: std::marker::PhantomData,
    }
}

impl Drop for RunSlotScope {
    fn drop(&mut self) {
        CURRENT_RUN_SLOT.with(|current| current.replace(self.previous.take()));
    }
}

/// The slot of the run on this thread, if it has one. Only event runs do.
pub fn current_run_slot() -> Option<RunSlotHandle> {
    CURRENT_RUN_SLOT.with(|current| current.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `body` on a blocking thread, as a VM runs.
    async fn on_vm_thread<R: Send + 'static>(body: impl FnOnce() -> R + Send + 'static) -> R {
        tokio::task::spawn_blocking(body).await.unwrap()
    }

    #[tokio::test]
    async fn a_slot_is_held_until_dropped() {
        let slots = RunSlots::new(1);
        let slot = slots.acquire().await;
        assert_eq!(slots.available(), 0);
        drop(slot);
        assert_eq!(slots.available(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_parked_run_lets_another_run_execute_and_then_continues() {
        let slots = RunSlots::new(1);
        let waiting = slots.acquire().await;
        let handle = waiting.handle();

        let parked = handle.park(None);
        assert_eq!(slots.available(), 1);
        let other = slots.acquire().await;

        let (resumed_tx, mut resumed_rx) = tokio::sync::oneshot::channel();
        let resume = tokio::task::spawn_blocking(move || {
            drop(parked);
            resumed_tx.send(()).unwrap();
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            resumed_rx.try_recv().is_err(),
            "a run continues only once it has a slot"
        );

        drop(other);
        resume.await.unwrap();
        assert_eq!(slots.available(), 0, "the run has its slot back");
        drop(waiting);
        assert_eq!(slots.available(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parallel_branches_give_the_slot_back_once() {
        let slots = RunSlots::new(2);
        let slot = slots.acquire().await;
        let handle = slot.handle();

        let first = handle.park(None);
        let second = handle.park(None);
        assert_eq!(slots.available(), 2);

        on_vm_thread(move || drop(first)).await;
        assert_eq!(slots.available(), 2, "another branch is still waiting");
        on_vm_thread(move || drop(second)).await;
        assert_eq!(slots.available(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_run_stops_without_queueing_for_a_slot() {
        let slots = RunSlots::new(1);
        let slot = slots.acquire().await;
        let cancel = Arc::new(AtomicBool::new(false));
        let parked = slot.handle().park(Some(Arc::clone(&cancel)));
        let other = slots.acquire().await;

        cancel.store(true, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(5), on_vm_thread(move || drop(parked)))
            .await
            .expect("a cancelled run must not wait for a slot");
        drop(other);
        assert_eq!(slots.available(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_vm_that_outlives_its_delivery_takes_no_slot() {
        let slots = RunSlots::new(1);
        let slot = slots.acquire().await;
        let parked = slot.handle().park(None);

        drop(slot);
        assert_eq!(slots.available(), 1);
        on_vm_thread(move || drop(parked)).await;
        assert_eq!(slots.available(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resuming_run_goes_ahead_of_runs_that_have_not_started() {
        let slots = RunSlots::new(1);
        let resuming = slots.acquire().await;
        let parked = resuming.handle().park(None);
        let holder = slots.acquire().await;

        let queued_slots = slots.clone();
        let queued = tokio::spawn(async move { queued_slots.acquire().await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let resume = tokio::task::spawn_blocking(move || drop(parked));
        tokio::time::sleep(Duration::from_millis(30)).await;

        drop(holder);
        tokio::time::timeout(Duration::from_secs(5), resume)
            .await
            .expect("the run that started first must get the slot")
            .unwrap();
        assert!(!queued.is_finished(), "the new run must still be waiting");

        drop(resuming);
        tokio::time::timeout(Duration::from_secs(5), queued)
            .await
            .expect("the new run gets the next free slot")
            .unwrap();
    }

    #[test]
    fn a_scope_sets_the_slot_for_its_thread_and_restores_the_last_one() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let slots = RunSlots::new(2);
        let outer = runtime.block_on(slots.acquire());
        let inner = runtime.block_on(slots.acquire());
        assert!(current_run_slot().is_none());
        let outer_scope = enter_run_slot(Some(outer.handle()));
        {
            let _inner_scope = enter_run_slot(Some(inner.handle()));
            let current = current_run_slot().unwrap();
            assert!(Arc::ptr_eq(&current.shared, &inner.handle().shared));
        }
        let current = current_run_slot().unwrap();
        assert!(Arc::ptr_eq(&current.shared, &outer.handle().shared));
        drop(outer_scope);
        assert!(current_run_slot().is_none());
    }
}

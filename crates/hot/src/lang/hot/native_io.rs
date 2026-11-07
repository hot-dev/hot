//! Cancellation/deadline scope for synchronous natives and their iterators.
//! Installed at VM dispatch, including when JIT calls back into the VM.
use super::r#type::HotResult;
use crate::lang::runtime::vm::VirtualMachine;
use crate::val::Val;
use std::cell::RefCell;
use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

/// Optional cap for native I/O outside a run deadline. Unset (the default)
/// means no implicit deadline: a local `hot run` can hold a LISTEN/SUBSCRIBE
/// loop or a long download open as long as it needs.
fn default_timeout() -> Option<Duration> {
    std::env::var("HOT_NATIVE_IO_TIMEOUT_SECONDS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
}

/// `None` when the wait is too large to represent: that is no deadline.
fn deadline_after(wait: Duration) -> Option<Instant> {
    Instant::now().checked_add(wait)
}

#[derive(Clone)]
pub(crate) struct IoContext {
    cancel: Option<Arc<AtomicBool>>,
    /// `None` means no deadline; cancellation still applies.
    deadline: Option<Instant>,
}
impl Default for IoContext {
    fn default() -> Self {
        Self {
            cancel: None,
            deadline: default_timeout().and_then(deadline_after),
        }
    }
}
impl IoContext {
    pub(crate) fn check(&self) -> Result<(), String> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|token| token.load(Ordering::Relaxed))
        {
            return Err("operation cancelled".to_string());
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err("operation deadline exceeded".to_string());
        }
        Ok(())
    }
    /// Time left before the deadline, or `None` when there is none.
    pub(crate) fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }
    /// How long one wait may block before re-checking cancellation and the
    /// deadline: the remaining time, capped at `poll`.
    pub(crate) fn poll_interval(&self, poll: Duration) -> Duration {
        self.remaining()
            .map_or(poll, |remaining| remaining.min(poll))
    }
}
thread_local! { static CONTEXT: RefCell<Option<IoContext>> = const { RefCell::new(None) }; }
pub(crate) struct Scope(Option<IoContext>, std::marker::PhantomData<std::rc::Rc<()>>);
impl Scope {
    pub(crate) fn enter(vm: &VirtualMachine) -> Self {
        let wait = vm
            .get_execution_context()
            .as_ref()
            .and_then(|context| context.deadline_at)
            .map(|deadline| (deadline - chrono::Utc::now()).to_std().unwrap_or_default())
            .or_else(default_timeout);
        let context = IoContext {
            cancel: vm.external_cancel_token(),
            deadline: wait.and_then(deadline_after),
        };
        Self(
            CONTEXT.with(|slot| slot.replace(Some(context))),
            std::marker::PhantomData,
        )
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        CONTEXT.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}
pub(crate) fn current() -> IoContext {
    CONTEXT.with(|slot| slot.borrow().clone().unwrap_or_default())
}

thread_local! {
    static BLOCKING_POOL: RefCell<Option<Arc<tokio::sync::Semaphore>>> = const { RefCell::new(None) };
}
/// Task-dependent waits must leave a physical VM thread available for the
/// queued task they may depend on. This is separate from CPU scheduling.
pub struct BlockingPoolScope(
    Option<Arc<tokio::sync::Semaphore>>,
    std::marker::PhantomData<std::rc::Rc<()>>,
);
impl BlockingPoolScope {
    pub fn enter(pool: Arc<tokio::sync::Semaphore>) -> Self {
        Self(
            BLOCKING_POOL.with(|slot| slot.replace(Some(pool))),
            std::marker::PhantomData,
        )
    }
}
impl Drop for BlockingPoolScope {
    fn drop(&mut self) {
        BLOCKING_POOL.with(|slot| slot.replace(self.0.take()));
    }
}
pub(crate) fn blocking_pool() -> Option<Arc<tokio::sync::Semaphore>> {
    BLOCKING_POOL.with(|slot| slot.borrow().clone())
}
pub(crate) fn park_task_wait() -> Result<Option<crate::run_slots::ParkedRun>, Val> {
    // The last physical execution cannot park on a task: earlier waiters can
    // only have observed a free permit before it was admitted. Ordinary remote
    // I/O has no dependency on another queued VM and does not need this gate.
    if blocking_pool().is_some_and(|pool| pool.available_permits() == 0) {
        return Err(Val::from(
            "Task wait would exhaust worker execution capacity; increase worker.max-blocking-executions",
        ));
    }
    park_current()
}

pub(crate) fn park_current() -> Result<Option<crate::run_slots::ParkedRun>, Val> {
    Ok(crate::run_slots::current_run_slot().map(|slot| slot.park(current().cancel)))
}

async fn bounded<F: Future>(operation: &str, future: F) -> Result<F::Output, Val> {
    let context = current();
    tokio::pin!(future);
    loop {
        context
            .check()
            .map_err(|e| Val::from(format!("{operation}: {e}")))?;
        tokio::select! {
            result = &mut future => return Ok(result),
            _ = tokio::time::sleep(context.poll_interval(Duration::from_millis(25))) => {}
        }
    }
}
pub(crate) fn block_on_result<F, T>(operation: &str, future: F) -> Result<T, Val>
where
    F: Future<Output = Result<T, Val>>,
{
    let _parked = park_current()?;
    tokio::runtime::Handle::current().block_on(bounded(operation, future))?
}
pub(crate) fn block_on_hot<F>(operation: &str, future: F) -> HotResult<Val>
where
    F: Future<Output = HotResult<Val>>,
{
    let _parked = match park_current() {
        Ok(parked) => parked,
        Err(error) => return HotResult::Err(error),
    };
    match tokio::runtime::Handle::current().block_on(bounded(operation, future)) {
        Ok(value) => value,
        Err(error) => HotResult::Err(error),
    }
}

/// Bound on release work after cancellation; see [`block_on_release`].
pub(crate) const RELEASE_BOUND: Duration = Duration::from_secs(2);

/// Release work (closing a socket, sending a close frame) runs even after the
/// run was cancelled or passed its deadline: skipping it would leak the
/// resource. It is bounded by `bound` instead. `None` when it did not finish.
pub(crate) fn block_on_release<F: Future>(bound: Duration, future: F) -> Option<F::Output> {
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    runtime.block_on(tokio::time::timeout(bound, future)).ok()
}

/// How long a stream wait blocks before re-checking cancellation and the
/// deadline. A message wakes the wait at once; this only bounds how late a
/// cancellation is noticed.
const RECV_POLL: Duration = Duration::from_millis(100);

/// The outcome of [`recv_blocking`].
pub(crate) enum Received<T> {
    Item(T),
    Closed,
    TimedOut,
}

/// Wait for the next message on `receiver`, waking as soon as one arrives,
/// and observing cancellation and the deadline at least every [`RECV_POLL`].
/// `until` is a caller timeout on top of those. Call it after parking and
/// before taking any lock around `receiver`: resuming from a park can wait
/// for a run slot, which must not happen while holding a lock another run
/// may need.
pub(crate) fn recv_blocking<T>(
    receiver: &mut tokio::sync::mpsc::Receiver<T>,
    until: Option<Instant>,
) -> Result<Received<T>, String> {
    let context = current();
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        // No runtime to time the wait: block as 3.0.4 did.
        context.check()?;
        return Ok(receiver
            .blocking_recv()
            .map_or(Received::Closed, Received::Item));
    };
    loop {
        context.check()?;
        let mut wait = context.poll_interval(RECV_POLL);
        if let Some(until) = until {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Received::TimedOut);
            }
            wait = wait.min(left);
        }
        match runtime.block_on(tokio::time::timeout(wait, receiver.recv())) {
            Ok(Some(item)) => return Ok(Received::Item(item)),
            Ok(None) => return Ok(Received::Closed),
            Err(_) => {}
        }
    }
}

#[cfg(test)]
pub(crate) fn test_scope(cancel: Option<Arc<AtomicBool>>, timeout: Duration) -> Scope {
    Scope(
        CONTEXT.with(|slot| {
            slot.replace(Some(IoContext {
                cancel,
                deadline: deadline_after(timeout),
            }))
        }),
        std::marker::PhantomData,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_waits_leave_capacity_for_their_dependency() {
        let pool = Arc::new(tokio::sync::Semaphore::new(2));
        let _scope = BlockingPoolScope::enter(pool.clone());
        let first = pool.clone().try_acquire_owned().unwrap();
        assert!(park_task_wait().is_ok());
        let last = pool.clone().try_acquire_owned().unwrap();
        assert!(park_task_wait().is_err());
        // Remote I/O does not depend on admitting another queued task.
        assert!(park_current().is_ok());
        drop(first);
        assert!(park_task_wait().is_ok());
        drop(last);
    }

    /// Outside a run deadline, native I/O has none unless one is configured,
    /// so a local LISTEN/SUBSCRIBE loop or long download isn't cut off. A
    /// wait too large to represent is no deadline, not a panic.
    #[test]
    fn native_io_has_no_implicit_deadline() {
        if std::env::var_os("HOT_NATIVE_IO_TIMEOUT_SECONDS").is_none() {
            assert!(current().remaining().is_none());
            assert!(IoContext::default().deadline.is_none());
        }
        let _scope = test_scope(None, Duration::MAX);
        assert!(current().check().is_ok());
        assert!(current().remaining().is_none());
    }

    /// A stream wait wakes on the message, not on a polling tick, and still
    /// notices a cancellation while nothing arrives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_waits_wake_on_data_and_observe_cancellation() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(4);
        let token = Arc::new(AtomicBool::new(false));
        let for_thread = token.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let _scope = test_scope(Some(for_thread), Duration::MAX);
            let mut got = Vec::new();
            for _ in 0..3 {
                match recv_blocking(&mut rx, None) {
                    Ok(Received::Item(item)) => got.push((item, Instant::now())),
                    Ok(_) => break,
                    Err(error) => return (got, Some(error)),
                }
            }
            let error = recv_blocking(&mut rx, None).err();
            (got, error)
        });
        let mut sent = Vec::new();
        for item in 0..3 {
            tokio::time::sleep(Duration::from_millis(30)).await;
            sent.push(Instant::now());
            tx.send(item).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.store(true, Ordering::Relaxed);
        let (got, error) = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            got.iter().map(|(item, _)| *item).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        // Each arrives well inside a 100 ms re-check, so the wait woke on it.
        for ((_, received), sent) in got.iter().zip(&sent) {
            assert!(received.duration_since(*sent) < Duration::from_millis(50));
        }
        assert!(error.unwrap().contains("cancelled"));
        drop(tx);
    }

    /// Release work still runs once the run is cancelled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn release_runs_after_cancellation() {
        let ran = tokio::task::spawn_blocking(|| {
            let _scope = test_scope(Some(Arc::new(AtomicBool::new(true))), Duration::ZERO);
            assert!(current().check().is_err());
            block_on_release(RELEASE_BOUND, async { true })
        })
        .await
        .unwrap();
        assert_eq!(ran, Some(true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_io_wait_respects_cancellation_and_deadline() {
        for cancel in [false, true] {
            let token = Arc::new(AtomicBool::new(false));
            let for_thread = token.clone();
            let handle = tokio::task::spawn_blocking(move || {
                let _scope = test_scope(
                    Some(for_thread),
                    Duration::from_millis(if cancel { 3000 } else { 40 }),
                );
                block_on_result::<_, ()>("test wait", std::future::pending())
            });
            if cancel {
                tokio::time::sleep(Duration::from_millis(30)).await;
                token.store(true, Ordering::Relaxed);
            }
            let error = tokio::time::timeout(Duration::from_secs(1), handle)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(if cancel { "cancelled" } else { "deadline" })
            );
        }
    }
}

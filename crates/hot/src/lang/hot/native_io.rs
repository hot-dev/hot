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

fn default_timeout() -> Duration {
    Duration::from_secs(crate::lang::runtime::limits::operational_limit(
        "HOT_NATIVE_IO_TIMEOUT_SECONDS",
        300,
    ) as u64)
}

#[derive(Clone)]
pub(crate) struct IoContext {
    cancel: Option<Arc<AtomicBool>>,
    deadline: Instant,
}
impl Default for IoContext {
    fn default() -> Self {
        Self {
            cancel: None,
            deadline: Instant::now() + default_timeout(),
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
        if Instant::now() >= self.deadline {
            return Err("operation deadline exceeded".to_string());
        }
        Ok(())
    }
    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
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
            .unwrap_or_else(default_timeout);
        let context = IoContext {
            cancel: vm.external_cancel_token(),
            deadline: Instant::now() + wait,
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
            _ = tokio::time::sleep(context.remaining().min(Duration::from_millis(25))) => {}
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

#[cfg(test)]
pub(crate) fn test_scope(cancel: Option<Arc<AtomicBool>>, timeout: Duration) -> Scope {
    Scope(
        CONTEXT.with(|slot| {
            slot.replace(Some(IoContext {
                cancel,
                deadline: Instant::now() + timeout,
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

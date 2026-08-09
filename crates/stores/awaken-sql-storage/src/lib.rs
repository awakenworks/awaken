//! Canonical synchronous-port to asynchronous-store runtime bridge.
//!
//! A few durable adapters implement pre-existing synchronous repository ports
//! with async database clients. This crate owns the one scheduling policy for
//! that boundary so adapters cannot independently introduce runtime starvation
//! or nested-`block_on` panics.

use std::future::Future;

use tokio::runtime::{Handle, RuntimeFlavor};

/// Drive a store future on the ambient Tokio runtime from a synchronous port.
///
/// `handle` must identify the runtime currently executing the caller. On a
/// multi-thread runtime, `block_in_place` yields the worker before blocking so
/// the database driver's tasks can continue. Calling a synchronous port from a
/// current-thread runtime cannot make progress safely and therefore fails fast
/// instead of deadlocking. Outside Tokio, a short-lived thread enters `handle`.
pub fn block_on_ambient_runtime<T, F, Fut>(handle: &Handle, make: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T>,
    T: Send + 'static,
{
    let handle = handle.clone();
    match Handle::try_current() {
        Ok(current) if matches!(current.runtime_flavor(), RuntimeFlavor::MultiThread) => {
            tokio::task::block_in_place(move || handle.block_on(make()))
        }
        Ok(_) => panic!(
            "synchronous async-store ports require a multi-thread Tokio runtime; current-thread runtimes would deadlock"
        ),
        Err(_) => std::thread::spawn(move || handle.block_on(make()))
            .join()
            .expect("ambient store runtime bridge thread panicked"),
    }
}

/// Drive a store future on a store-owned runtime from any caller runtime.
///
/// The owned runtime has independent workers and I/O drivers, so a fresh thread
/// may synchronously enter it without occupying or nesting inside the caller's
/// runtime. `make` constructs the future on that thread and need not be `Send`.
pub fn block_on_owned_runtime<T, F, Fut>(handle: &Handle, make: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T>,
    T: Send + 'static,
{
    let handle = handle.clone();
    std::thread::spawn(move || handle.block_on(make()))
        .join()
        .expect("owned store runtime bridge thread panicked")
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use tokio::runtime::Builder;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn ambient_bridge_yields_its_only_runtime_worker() {
        // Cause/effect graph: C1 port uses the ambient runtime; C2 call begins
        // inside/outside Tokio; C3 ambient runtime is multi/current-thread.
        // Effects: E1 completes with replacement capacity; E2 enters the live
        // runtime from a helper thread; E3 fails fast instead of deadlocking.
        //
        // | Rule | C1 ambient | C2 inside | C3 flavor       | Effect |
        // |---|---|---|---|---|
        // | D1 | T | T | multi-thread(1) | E1 |
        // | D2 | T | F | multi-thread    | E2 (covered below) |
        // | D3 | T | T | current-thread  | E3 (covered below) |
        let scheduled = tokio::spawn(async { 42_u8 });
        let value = block_on_ambient_runtime(&Handle::current(), move || async move {
            scheduled.await.expect("scheduled store future")
        });
        assert_eq!(value, 42, "D1");
    }

    #[test]
    fn ambient_bridge_enters_a_live_runtime_from_a_plain_thread() {
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let value = block_on_ambient_runtime(runtime.handle(), || async { 42_u8 });
        assert_eq!(value, 42, "D2");
    }

    #[test]
    fn ambient_bridge_rejects_a_current_thread_runtime() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let result = catch_unwind(AssertUnwindSafe(|| {
            runtime.block_on(async {
                block_on_ambient_runtime(&Handle::current(), || async { 42_u8 })
            })
        }));
        assert!(result.is_err(), "D3");
    }

    #[test]
    fn owned_bridge_is_independent_of_a_current_thread_caller() {
        // Owned-runtime decision rule: a distinct runtime with an active worker
        // always uses the helper-thread path, regardless of the caller flavor.
        // This preserves E1 completion without treating it as ambient capacity.
        let owned = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("owned runtime");
        let caller = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("caller runtime");
        let target = owned.handle().clone();
        let scheduled = target.spawn(async { 42_u8 });
        let value = caller.block_on(async move {
            block_on_owned_runtime(&target, move || async move {
                scheduled.await.expect("owned store future")
            })
        });
        assert_eq!(value, 42);
    }
}

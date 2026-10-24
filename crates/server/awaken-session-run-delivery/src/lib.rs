//! `awaken-session-run-delivery` — how a Coordinator delivers a Session's Runs
//! to the Workers that execute them and observes them settle.
//!
//! A Coordinator admits a Run into dispatch, a registered Worker claims and
//! executes it, and the Worker's settlement and live progress come back over
//! the Worker transport. This crate owns the Coordinator half of that loop
//! that needs no execution machinery: the per-Thread live observation point
//! ([`ThreadEventHub`]) every protocol adapter subscribes to, and the
//! foreground completion relay ([`CompletionRegistry`]) a caller registers
//! with before its Run is activated. It links no Session realization, no
//! Sandbox and no credential realization, so the Coordinator process composes
//! it without the Worker-side executor; the executor builds on it for the
//! Runs a co-located pool executes.
//!
//! Neither component is durable truth: committed Run truth decides every
//! outcome, these only notify and preview.
#![warn(unreachable_pub)]

mod completion;
mod hub;
mod lock;

pub use completion::{
    CompletionRegistry, CompletionSignalClosed, WaiterGuard, activate_and_await_session_run,
    await_completion_state,
};
pub use hub::{ThreadEvent, ThreadEventHub};
pub use lock::{DeliveryLock, LOCK_POISONED_CODE};

/// Poison `lock` the way production can: a holder panics while holding it.
#[cfg(test)]
pub(crate) fn poison_for_test<T>(lock: &std::sync::Mutex<T>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = lock.lock();
        panic!("poison the lock under test");
    }));
    assert!(lock.is_poisoned());
}

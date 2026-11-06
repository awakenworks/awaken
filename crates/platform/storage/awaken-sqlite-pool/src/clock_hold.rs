// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Static owner: the paused-clock visibility of embedded SQLite work (ADR-0087). Dynamic
// @formal reason: path: sqlx runs SQLite on its own thread; a held blocking task keeps a paused Tokio clock
// @formal reason: from jumping over that work, as the former blocking bridge did for every operation.
// @formal reason: Evidence: the pool's paused-clock decision-table test (C1-C3); this is a
// @formal reason: product-requirement classification, not a direct formal-proof claim.
//! Keeping a paused Tokio clock still while SQLite works.
//!
//! A paused clock jumps to the next timer as soon as its runtime looks idle. sqlx
//! runs SQLite on its own thread, which the runtime cannot see, so without help a
//! timer would fire while a statement is still running. A running blocking task
//! is visible to the runtime and stops that jump. Only a current-thread runtime
//! can pause its clock, so any other runtime pays nothing.

use std::future::Future;

pub(crate) struct ClockHold {
    _release: std::sync::mpsc::Sender<()>,
}

impl ClockHold {
    pub(crate) fn take() -> Option<Self> {
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        if runtime.runtime_flavor() != tokio::runtime::RuntimeFlavor::CurrentThread {
            return None;
        }
        let (release, released) = std::sync::mpsc::channel::<()>();
        drop(runtime.spawn_blocking(move || {
            let _ = released.recv();
        }));
        Some(Self { _release: release })
    }
}

/// Run SQLite work that does not go through [`crate::EmbeddedPool`], such as a
/// schema migration on [`crate::EmbeddedPool::sqlx`], with the clock held.
pub async fn held<F: Future>(work: F) -> F::Output {
    let _hold = ClockHold::take();
    work.await
}

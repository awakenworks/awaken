// @formal surface obligation=session_execution.one_physical_attempt_per_thread
// @formal reason: Process-local Worker placement measurement enters only after the existing persisted
// @formal reason: physical-attempt admission succeeds and leaves when that admitted scope returns. The
// @formal reason: authoritative Dispatch slot and its existing proof remain unchanged; adjacent Pool
// @formal reason: decision-table tests cover resolver, Runtime and return boundaries. This link classifies
// @formal reason: executable concurrency evidence and does not promote atomic scheduling, heartbeat
// @formal reason: sampling or registry placement to a formal proof.
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Process-local count of Runs that crossed physical-attempt admission and may
/// execute model, tool, or Sandbox work. Claim resolution and predecessor
/// observation deliberately do not enter this counter.
#[derive(Clone, Default)]
pub(crate) struct ExecutionCapacity(Arc<AtomicU32>);

impl ExecutionCapacity {
    pub(crate) fn enter(&self) -> ExecutionCapacityGuard {
        self.0.fetch_add(1, Ordering::SeqCst);
        ExecutionCapacityGuard(self.0.clone())
    }

    pub(crate) fn in_flight(&self) -> u32 {
        self.0.load(Ordering::SeqCst)
    }
}

pub(crate) struct ExecutionCapacityGuard(Arc<AtomicU32>);

impl Drop for ExecutionCapacityGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

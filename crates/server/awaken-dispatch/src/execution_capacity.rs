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

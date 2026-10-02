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
pub struct ExecutionCapacity(Arc<AtomicU32>);

impl ExecutionCapacity {
    #[must_use]
    pub fn enter(&self) -> ExecutionCapacityGuard {
        self.0.fetch_add(1, Ordering::SeqCst);
        ExecutionCapacityGuard(self.0.clone())
    }

    #[must_use]
    pub fn in_flight(&self) -> u32 {
        self.0.load(Ordering::SeqCst)
    }
}

pub struct ExecutionCapacityGuard(Arc<AtomicU32>);

impl Drop for ExecutionCapacityGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::ExecutionCapacity;

    /// Causes: C1=one physical attempt enters through the capacity owner;
    /// C2=another owner clone observes it; C3=the exact guard returns.
    /// Effects: E1=C1+C2 reports one live attempt without a second counter;
    /// E2=C3 returns both views to zero. Decision rules R1=C1+C2 -> E1,
    /// R2=R1+C3 -> E2. Claim resolution never calls `enter` and is covered by
    /// the process-pool admission tests at the caller boundary.
    #[test]
    fn cloned_capacity_counts_only_the_live_physical_attempt_guard() {
        let capacity = ExecutionCapacity::default();
        let observed = capacity.clone();
        assert_eq!(capacity.in_flight(), 0);
        let guard = capacity.enter();
        assert_eq!(capacity.in_flight(), 1, "R1");
        assert_eq!(observed.in_flight(), 1, "R1 shared owner");
        drop(guard);
        assert_eq!(capacity.in_flight(), 0, "R2");
        assert_eq!(observed.in_flight(), 0, "R2 shared owner");
    }
}

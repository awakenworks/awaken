//! Coordinator-owned Worker ranking policy.
//!
//! Eligibility remains the pure kernel in `awaken-worker-registry`; this module
//! owns only the process-wide replaceable ranking policy used by dispatch claims.

use std::sync::{Arc, OnceLock, RwLock};

use awaken_worker_registry::{
    PlacementContext, PlacementError, PlacementPolicy, RankedWorker, WorkerSnapshot,
};

/// Atomically replaceable policy slot. A replacement may rank only candidates
/// already admitted by the Worker contract's compatibility predicate.
pub struct ReplaceablePlacementPolicy {
    policy: RwLock<Arc<dyn PlacementPolicy>>,
}

impl PlacementPolicy for ReplaceablePlacementPolicy {
    fn id(&self) -> &str {
        "replaceable"
    }

    fn rank(
        &self,
        context: &PlacementContext,
        eligible: &[WorkerSnapshot],
    ) -> Result<Vec<RankedWorker>, PlacementError> {
        self.policy
            .read()
            .expect("placement policy slot poisoned")
            .rank(context, eligible)
    }
}

static SHARED_POLICY: OnceLock<Arc<ReplaceablePlacementPolicy>> = OnceLock::new();

/// The Coordinator's one Worker-ranking policy slot.
#[must_use]
pub fn shared_worker_placement_policy() -> Arc<ReplaceablePlacementPolicy> {
    SHARED_POLICY
        .get_or_init(|| {
            ReplaceablePlacementPolicy::new(Arc::new(awaken_worker_registry::LeastLoadedPolicy))
        })
        .clone()
}

impl ReplaceablePlacementPolicy {
    #[must_use]
    pub fn new(policy: Arc<dyn PlacementPolicy>) -> Arc<Self> {
        Arc::new(Self {
            policy: RwLock::new(policy),
        })
    }

    #[must_use]
    pub fn active_id(&self) -> String {
        self.policy
            .read()
            .expect("placement policy slot poisoned")
            .id()
            .to_string()
    }

    pub fn replace(&self, policy: Arc<dyn PlacementPolicy>) -> Arc<dyn PlacementPolicy> {
        std::mem::replace(
            &mut *self.policy.write().expect("placement policy slot poisoned"),
            policy,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EmptyPolicy;

    impl PlacementPolicy for EmptyPolicy {
        fn id(&self) -> &str {
            "empty"
        }

        fn rank(
            &self,
            _context: &PlacementContext,
            _eligible: &[WorkerSnapshot],
        ) -> Result<Vec<RankedWorker>, PlacementError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn replacement_has_one_authoritative_policy_slot() {
        // Cause/effect graph: C1=initial policy installed; C2=replacement occurs.
        // E1=active id reflects the only current policy; E2=the replaced policy is
        // returned to its owner. Decision rules: R1 C1&&!C2 -> least-loaded;
        // R2 C1&&C2 -> empty + previous least-loaded.
        let policy =
            ReplaceablePlacementPolicy::new(Arc::new(awaken_worker_registry::LeastLoadedPolicy));
        assert_eq!(policy.active_id(), "least-loaded", "R1");
        let previous = policy.replace(Arc::new(EmptyPolicy));
        assert_eq!(previous.id(), "least-loaded", "R2 previous");
        assert_eq!(policy.active_id(), "empty", "R2 active");
    }
}

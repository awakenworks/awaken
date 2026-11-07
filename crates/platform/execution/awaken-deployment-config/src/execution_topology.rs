// @formal surface exclude=composition_wiring
// @formal reason: Plain topology values the composition root selects once; durable claim and ingress
// @formal reason: decisions belong to ProcessDispatch and the dispatch store, which carry their own proofs.
//! How this process takes part in Run execution, decided by the composition
//! root before the Session executor exists (ADR-0091 D6: only process compositions read
//! deployment configuration).
//!
//! Dispatch never derives these choices from a configuration file. Each value
//! names the one decision it carries: whether a foreground Run is delivered
//! through the durable queue ([`ForegroundRunDelivery`]), whether this process drains that
//! queue itself and with how many slots ([`ClaimPool`]), the identity its
//! dispatch leases are fenced with ([`LeaseOwnerId`]), how long a lost wake can
//! delay recovery, and who observes queue depth ([`QueueDepthSource`]).

/// The default lost-wake recovery bound: cross-node wake drives prompt work,
/// so fallback reads need not hit the durable queue twenty times per second
/// while it is idle. Consumers: [`crate::DeploymentConfig::ephemeral`] and the
/// dispatch service's default schedule.
pub const DEFAULT_DISPATCH_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(500);

/// Whether a foreground Run is delivered through the durable dispatch queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundRunDelivery {
    /// The submitting Session drives its attempt directly; queued, crashed and
    /// scheduled work is not recovered across a restart.
    Direct,
    /// Every Run is enqueued and claimed from the durable dispatch queue, and
    /// the durable operational verbs (cancel, requeue, inspect) are admitted.
    Durable,
}

impl ForegroundRunDelivery {
    #[must_use]
    pub const fn is_durable(self) -> bool {
        matches!(self, Self::Durable)
    }
}

/// How the one process-level dispatch pool got its slot count.
///
/// The two paths are deliberately distinct variants instead of one integer with
/// a default baked into the constructor. A registered Worker publishes
/// `WorkerCapacity::max_concurrent` to the registry and the scheduler places
/// against it; installing a locally guessed number instead is exactly the
/// "registered value nobody consumed" divergence, and it used to be invisible
/// because both numbers had the same type. Now the CPU guess is a named branch
/// that only an unregistered composition produces, and [`Registered`] is
/// produced by moving a [`RegisteredCapacity`] into
/// the Session executor's Worker constructor.
///
/// [`Registered`]: DispatchPoolSizing::Registered
/// [`RegisteredCapacity`]: awaken_worker_contract::RegisteredCapacity
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchPoolSizing {
    /// No Worker registration in this topology (a direct or coordinator-embedded
    /// process). No registry ever published a capacity for this process, so local
    /// parallelism is the whole truth and cannot contradict anything.
    LocalParallelism(std::num::NonZeroUsize),
    /// A registered Worker installed its validated manifest capacity. This is
    /// the number the registry handed the scheduler, so it is also the exact
    /// number of execution slots the pool installs.
    Registered(std::num::NonZeroUsize),
}

impl DispatchPoolSizing {
    /// Slots to install. Both branches are resolved here so no caller can read
    /// one variant and forget the other.
    #[must_use]
    pub const fn slots(self) -> std::num::NonZeroUsize {
        match self {
            Self::LocalParallelism(value) | Self::Registered(value) => value,
        }
    }

    /// The registered capacity when this process is a registered Worker.
    #[must_use]
    pub const fn registered(self) -> Option<std::num::NonZeroUsize> {
        match self {
            Self::Registered(value) => Some(value),
            Self::LocalParallelism(_) => None,
        }
    }

    /// The CPU-derived sizing used by a process that registered no capacity.
    #[must_use]
    pub fn local_parallelism() -> Self {
        Self::LocalParallelism(
            std::thread::available_parallelism()
                .unwrap_or(std::num::NonZeroUsize::new(4).expect("nonzero local fallback")),
        )
    }
}

/// Whether this process executes claimed Runs itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimPool {
    /// One process-level dispatch pool with this many slots drains the queue
    /// here, and Session Environments may be realized locally.
    InProcess(DispatchPoolSizing),
    /// Admission only: this process owns the queue but never executes a Run;
    /// registered remote Workers are the sole drainers.
    RemoteWorkersOnly,
}

impl ClaimPool {
    /// The CPU-sized in-process pool of an unregistered process.
    #[must_use]
    pub fn in_process() -> Self {
        Self::InProcess(DispatchPoolSizing::local_parallelism())
    }

    #[must_use]
    pub const fn runs_local_pool(self) -> bool {
        matches!(self, Self::InProcess(_))
    }

    /// The pool sizing when this process executes Runs.
    #[must_use]
    pub const fn pool_sizing(self) -> Option<DispatchPoolSizing> {
        match self {
            Self::InProcess(sizing) => Some(sizing),
            Self::RemoteWorkersOnly => None,
        }
    }
}

/// The dispatch lease owner this process fences claims with: distinct per
/// process and node, stable across a restart of the same logical Worker.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LeaseOwnerId(String);

impl LeaseOwnerId {
    /// Refuse a blank owner: an empty identity cannot fence anything.
    pub fn new(owner: impl Into<String>) -> Result<Self, String> {
        let owner = owner.into();
        if owner.trim().is_empty() {
            return Err("dispatch_owner must be non-empty".to_owned());
        }
        Ok(Self(owner))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Who observes the dispatch queue depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueDepthSource {
    /// The pool samples the queue depth itself (standalone deployments).
    BuiltInSampler,
    /// A hosting composition already samples the same queue for its scaler, so
    /// this process takes no second reading.
    External,
}

/// The process's part in Run execution, selected once by its composition root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionTopology {
    pub foreground_delivery: ForegroundRunDelivery,
    pub claim_pool: ClaimPool,
    pub lease_owner: LeaseOwnerId,
    /// Maximum fallback wait before a pool checks durable dispatch again after
    /// a lost wake. The cross-node wake is only a hint; this is the recovery
    /// bound. Between 1 ms and 60 s.
    pub lost_wake_recovery_bound: std::time::Duration,
    pub queue_depth: QueueDepthSource,
}

impl ExecutionTopology {
    /// The one admission rule for the recovery bound, applied by the executor
    /// constructor and by every composition that parses it.
    pub fn validate(&self) -> Result<(), String> {
        if self.lost_wake_recovery_bound.is_zero()
            || self.lost_wake_recovery_bound > std::time::Duration::from_secs(60)
        {
            return Err("dispatch_poll_interval_ms must be between 1 and 60000".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A standalone process: direct ingress, CPU-sized pool, built-in sampler.
    fn standalone() -> ExecutionTopology {
        ExecutionTopology {
            foreground_delivery: ForegroundRunDelivery::Direct,
            claim_pool: ClaimPool::in_process(),
            lease_owner: LeaseOwnerId::new("node-a").unwrap(),
            lost_wake_recovery_bound: std::time::Duration::from_millis(250),
            queue_depth: QueueDepthSource::BuiltInSampler,
        }
    }

    #[test]
    fn recovery_bound_has_one_validated_projection() {
        // C1 a positive bound within 60s; C2 zero or more than 60s. E1 the
        // bound is admitted (its projection into the dispatch schedule is
        // owned and tested by dispatch-execution); E2 invalid values fail
        // before a process starts. Rules: D1=C1 -> E1, D2=C2 -> E2.
        let mut topology = standalone();
        topology.lost_wake_recovery_bound = std::time::Duration::from_millis(750);
        topology.validate().expect("D1 valid");
        for invalid in [0, 60_001] {
            topology.lost_wake_recovery_bound = std::time::Duration::from_millis(invalid);
            assert!(
                topology
                    .validate()
                    .unwrap_err()
                    .contains("dispatch_poll_interval_ms"),
                "D2/E2"
            );
        }
    }

    #[test]
    fn lease_owner_and_local_execution_are_total() {
        // Owner: blank -> refused; non-blank -> kept verbatim. Local execution:
        // only an in-process pool has sizing and drains locally.
        // Naming-only migration: Durable -> durable delivery, Direct -> direct
        // delivery. The original variants and predicate remain the same; the
        // crate's retired-path doctest refuses the former RunIngress facade.
        assert!(LeaseOwnerId::new("  ").is_err());
        assert_eq!(LeaseOwnerId::new("node-a").unwrap().as_str(), "node-a");
        let sizing = DispatchPoolSizing::Registered(std::num::NonZeroUsize::new(3).unwrap());
        assert_eq!(ClaimPool::InProcess(sizing).pool_sizing(), Some(sizing));
        assert!(ClaimPool::InProcess(sizing).runs_local_pool());
        assert_eq!(ClaimPool::RemoteWorkersOnly.pool_sizing(), None);
        assert!(!ClaimPool::RemoteWorkersOnly.runs_local_pool());
        assert!(
            ForegroundRunDelivery::Durable.is_durable()
                && !ForegroundRunDelivery::Direct.is_durable()
        );
    }
}

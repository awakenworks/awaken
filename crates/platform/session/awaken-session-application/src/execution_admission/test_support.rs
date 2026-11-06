// @formal surface exclude=test_only
// @formal owner crates/platform/session/awaken-session-application/src/tests/construction.rs
// @formal reason: The one existing recording dependency, compiled only under test or test-support;
// @formal reason: its callbacks record reads and carry no durable Registry, execution or placement authority.
//! The existing three-role recording fixture, shared by explicit test composition.
//! It is never selected by production construction and carries no registry truth.
use awaken_session_contract::PersistedSession;

/// Shared fixtures provide Registry observations and the placement compiler;
/// the Dispatch feed comes from its selected canonical store. The original
/// recording feed implementation exists only in this crate's unit tests.
///
/// C1 these three read/compiler roles are supported; C2 a shared dependency
/// asks this probe for Coordinator Dispatch authority. P1 C1 is well typed;
/// P2 C2 is rejected rather than creating a second operational-feed owner.
/// These are source-capability controls, not runtime or persistence evidence.
///
/// ```
/// use awaken_session_application::ExecutionObservationProbe;
/// fn selected<T: awaken_worker_contract::WorkerObservationSource
///     + awaken_worker_contract::WorkerIncarnationSource
///     + awaken_session_execution_contract::SessionPreparationPlacement>() {}
/// selected::<ExecutionObservationProbe>();
/// ```
///
/// ```compile_fail,E0277
/// use awaken_session_application::ExecutionObservationProbe;
/// fn feed<T: awaken_dispatch_contract::DispatchOperationalFeed>() {}
/// feed::<ExecutionObservationProbe>();
/// ```
#[derive(Default)]
pub struct ExecutionObservationProbe(pub std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerObservationSource for ExecutionObservationProbe {
    async fn list(
        &self,
    ) -> Result<Vec<awaken_worker_contract::RegisteredWorker>, awaken_worker_contract::RegistryError>
    {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Vec::new())
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerIncarnationSource for ExecutionObservationProbe {
    async fn incarnation(
        &self,
        _identity: &awaken_worker_contract::WorkerIdentity,
    ) -> Result<
        Option<awaken_worker_contract::RegisteredWorker>,
        awaken_worker_contract::RegistryError,
    > {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(None)
    }
}

impl awaken_session_execution_contract::SessionPreparationPlacement for ExecutionObservationProbe {
    fn requirements(
        &self,
        _session: &PersistedSession,
    ) -> Result<
        awaken_worker_contract::PlacementRequirements,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(awaken_worker_contract::PlacementRequirements::remote_required())
    }
}

// @formal surface obligation=work.lease_authority_snapshot_and_owner_mutations_are_atomic
// @formal reason: Composition projects required role views from the same backend without state or effects.
//! Required Work storage roles, assembled once at the composition boundary.
use awaken_executable_environment_contract as work;
use std::sync::Arc;

/// Views of one backend. Construction requires every capability; a query-only
/// adapter cannot silently become a Worker or a registration backend.
///
/// ```compile_fail
/// use std::sync::Arc;
/// use awaken_executable_environment_contract::WorkQueries;
/// use awaken_environment_application::execution::WorkQueue;
/// fn incomplete<T: WorkQueries + 'static>(backend: Arc<T>) {
///     let _: WorkQueue = backend.into();
/// }
/// ```
#[derive(Clone)]
pub struct WorkQueue {
    pub(super) queries: Arc<dyn work::WorkQueries>,
    pub(super) consumer: Arc<dyn work::WorkConsumer>,
    pub(super) authentication: Arc<dyn work::SessionAccessAuthenticator>,
    pub(super) session_control: Arc<dyn work::SessionWorkLeaseControl>,
    pub(super) assignment: Arc<dyn work::SessionWorkStore>,
    pub(super) worker_leases: Arc<dyn work::SessionWorkerLeaseStore>,
    pub(super) observation: Arc<dyn work::WorkerLeaseObservation>,
    pub(super) metadata: Arc<dyn work::WorkMetadataPatch>,
    pub(super) lifecycle: Arc<dyn work::EnvironmentWorkLifecycle>,
}
impl WorkQueue {
    #[must_use]
    pub fn queries(&self) -> Arc<dyn work::WorkQueries> {
        self.queries.clone()
    }
    #[must_use]
    pub fn consumer(&self) -> Arc<dyn work::WorkConsumer> {
        self.consumer.clone()
    }
    #[must_use]
    pub fn authentication(&self) -> Arc<dyn work::SessionAccessAuthenticator> {
        self.authentication.clone()
    }
    #[must_use]
    pub fn session_control(&self) -> Arc<dyn work::SessionWorkLeaseControl> {
        self.session_control.clone()
    }
    #[must_use]
    pub fn assignment(&self) -> Arc<dyn work::SessionWorkStore> {
        self.assignment.clone()
    }
    #[must_use]
    pub fn worker_leases(&self) -> Arc<dyn work::SessionWorkerLeaseStore> {
        self.worker_leases.clone()
    }
    #[must_use]
    pub fn observation(&self) -> Arc<dyn work::WorkerLeaseObservation> {
        self.observation.clone()
    }
    #[must_use]
    pub fn metadata(&self) -> Arc<dyn work::WorkMetadataPatch> {
        self.metadata.clone()
    }
    #[must_use]
    pub fn lifecycle(&self) -> Arc<dyn work::EnvironmentWorkLifecycle> {
        self.lifecycle.clone()
    }
}
impl<T> From<Arc<T>> for WorkQueue
where
    T: work::WorkQueries
        + work::WorkConsumer
        + work::SessionAccessAuthenticator
        + work::SessionWorkLeaseControl
        + work::SessionWorkStore
        + work::SessionWorkerLeaseStore
        + work::WorkerLeaseObservation
        + work::WorkMetadataPatch
        + work::EnvironmentWorkLifecycle
        + 'static,
{
    fn from(backend: Arc<T>) -> Self {
        Self {
            queries: backend.clone(),
            consumer: backend.clone(),
            authentication: backend.clone(),
            session_control: backend.clone(),
            assignment: backend.clone(),
            worker_leases: backend.clone(),
            observation: backend.clone(),
            metadata: backend.clone(),
            lifecycle: backend.clone(),
        }
    }
}

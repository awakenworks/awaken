//! Registration-bound application extension assembly.

use std::sync::Arc;

use awaken_runtime_host::AttemptExecutorDecorator;
use awaken_session_contract::ApplicationSessionProvisioner;
use awaken_worker_contract::{RegisteredWorker, WorkerIdentity, WorkerSnapshot};
use awaken_worker_transport_security::WorkerUpstream;

/// Immutable application assembly context created only after the Coordinator
/// allocates this process's Worker identity.
#[derive(Clone)]
pub struct RegisteredWorkerContext {
    registration: RegisteredWorker,
    upstream: WorkerUpstream,
}

impl RegisteredWorkerContext {
    pub(crate) fn new(registration: RegisteredWorker, upstream: WorkerUpstream) -> Self {
        Self {
            registration,
            upstream,
        }
    }

    #[must_use]
    pub fn registration(&self) -> &RegisteredWorker {
        &self.registration
    }

    #[must_use]
    pub fn snapshot(&self) -> &WorkerSnapshot {
        &self.registration.snapshot
    }

    #[must_use]
    pub fn identity(&self) -> &WorkerIdentity {
        &self.registration.snapshot.identity
    }

    /// The identity-bound transport shared by dispatch, claimed commit, and
    /// application-owned control requests.
    #[must_use]
    pub fn upstream(&self) -> &WorkerUpstream {
        &self.upstream
    }
}

/// The complete application assembly created from one registered Worker identity.
///
/// The provisioner adds claim-bound material to the Host's authoritative Session
/// environment; the decorator wraps the authoritative Native/ACP/A2A router.
pub struct RegisteredWorkerApplication {
    decorator: AttemptExecutorDecorator,
    session_provisioner: Option<Arc<dyn ApplicationSessionProvisioner>>,
}

impl RegisteredWorkerApplication {
    #[must_use]
    pub fn new(decorator: AttemptExecutorDecorator) -> Self {
        Self {
            decorator,
            session_provisioner: None,
        }
    }

    #[must_use]
    pub fn with_session_provisioner(
        mut self,
        provisioner: Arc<dyn ApplicationSessionProvisioner>,
    ) -> Self {
        self.session_provisioner = Some(provisioner);
        self
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        AttemptExecutorDecorator,
        Option<Arc<dyn ApplicationSessionProvisioner>>,
    ) {
        (self.decorator, self.session_provisioner)
    }
}

/// Registration-time application factory.
pub type RegisteredApplicationFactory = Arc<
    dyn Fn(&RegisteredWorkerContext) -> Result<RegisteredWorkerApplication, String> + Send + Sync,
>;

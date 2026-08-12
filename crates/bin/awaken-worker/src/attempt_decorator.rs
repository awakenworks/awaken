//! Registration-bound attempt decoration assembly.

use std::sync::Arc;

use awaken_runtime_host::AttemptExecutorDecorator;
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
    /// optional attempt-decoration control requests.
    #[must_use]
    pub fn upstream(&self) -> &WorkerUpstream {
        &self.upstream
    }
}

/// Registration-time factory for the one neutral attempt decorator. It cannot
/// author Session state, place Work, or replace the backend executor registry.
pub type RegisteredAttemptDecoratorFactory =
    Arc<dyn Fn(&RegisteredWorkerContext) -> Result<AttemptExecutorDecorator, String> + Send + Sync>;

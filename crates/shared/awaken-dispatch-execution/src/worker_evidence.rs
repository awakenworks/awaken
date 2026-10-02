//! Exact closed-idle Worker evidence release port for the process maintenance scan.

use awaken_run_ingress_application_contract::ApplicationError;
use awaken_dispatch_contract::{ControlDispatchQueue, WorkerDirectory, WorkerIdentity};

/// The Host scans and expires Registry records; Coordinator Ingress retains the
/// sole physical-stop and exact evidence-release implementation. This port
/// carries no process-exit receipt, closed-attempt command or second stop rule.
#[async_trait::async_trait]
pub trait WorkerEvidenceRelease: Send + Sync {
    /// Provider retention is optional. Registry expiry is independent of it.
    fn owns_worker_evidence(&self, identity: &WorkerIdentity) -> bool;

    /// Release only the original closed incarnation after the exact physical
    /// slot scan; unavailable or unprovable closure remains retryable.
    async fn release_closed_idle_worker_evidence(
        &self,
        dispatch: &dyn ControlDispatchQueue,
        directory: &dyn WorkerDirectory,
        identity: &WorkerIdentity,
    ) -> Result<(), ApplicationError>;
}

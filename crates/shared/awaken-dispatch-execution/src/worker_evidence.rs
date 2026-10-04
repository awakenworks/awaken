//! Exact closed-idle Worker retirement port for the process maintenance scan.

use awaken_run_ingress_application_contract::ApplicationError;
use awaken_dispatch_contract::{ControlDispatchQueue, WorkerDirectory, WorkerIdentity};

/// The Host scans and expires Registry records; Coordinator Ingress retains the
/// sole physical-stop and exact evidence-release implementation. This port
/// carries no process-exit receipt, closed-attempt command or second stop rule.
#[async_trait::async_trait]
pub trait WorkerEvidenceRelease: Send + Sync {
    /// Provider retention is optional. Registry expiry is independent of it.
    fn owns_worker_evidence(&self, identity: &WorkerIdentity) -> bool;

    /// Retire only the original closed-idle domain after the exact physical slot
    /// scan. The Host supplies its trusted clock so Ingress honors the original
    /// Registry lease deadline before stopping a still-active Worker. Unknown
    /// closure remains retryable; no exit receipt or attempt-settlement authority
    /// crosses this port.
    async fn release_closed_idle_worker_evidence(
        &self,
        dispatch: &dyn ControlDispatchQueue,
        directory: &dyn WorkerDirectory,
        identity: &WorkerIdentity,
        now_ms: u64,
    ) -> Result<(), ApplicationError>;
}

//! The dispatch ports now live in `awaken-run-ingress-contract` (ADR-0039 2.1),
//! re-exported here so the host's internal `crate::dispatch::*` paths and existing
//! consumers are unchanged.

pub use awaken_run_ingress_contract::dispatch::*;

pub(crate) fn installed_worker_credential_capabilities(
    worker: &crate::WorkerSnapshot,
) -> Result<awaken_runtime_contract::CredentialRealizationCapabilities, DispatchError> {
    awaken_run_ingress_contract::worker_credential_realization_capabilities(worker).map_err(
        |error| DispatchError::Rejected(format!("Worker capability evidence is invalid: {error}")),
    )
}

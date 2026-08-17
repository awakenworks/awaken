//! The dispatch ports now live in `awaken-run-ingress-contract` (ADR-0039 2.1),
//! re-exported here so the host's internal `crate::dispatch::*` paths and existing
//! consumers are unchanged.

pub use awaken_run_ingress_contract::dispatch::*;

use awaken_run_ingress_contract::RunDispatch;

/// Persistence-independent evidence available when one caller-owned Run id is
/// admitted. Backends load evidence; this function owns the one identity
/// decision so memory, SQLite, and Postgres cannot drift.
pub(crate) enum StoredRunIdentity<'a> {
    Absent,
    Live(&'a RunDispatch),
    Completed(Option<&'a str>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunIdentityDecision {
    New,
    Replay,
}

pub(crate) fn decide_run_identity(
    stored: StoredRunIdentity<'_>,
    incoming: &RunDispatch,
) -> Result<RunIdentityDecision, DispatchError> {
    let replay = match stored {
        StoredRunIdentity::Absent => return Ok(RunIdentityDecision::New),
        StoredRunIdentity::Live(existing) => existing.same_canonical_dispatch(incoming),
        StoredRunIdentity::Completed(Some(existing)) => {
            existing == incoming.canonical_fingerprint()
        }
        StoredRunIdentity::Completed(None) => false,
    };
    if replay {
        Ok(RunIdentityDecision::Replay)
    } else {
        Err(DispatchError::Rejected(format!(
            "run id `{}` was reused with another or unverifiable dispatch payload",
            incoming.run_id().0
        )))
    }
}

#[cfg(feature = "durable")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExactClaimMode {
    Runnable,
    TerminalRecovery,
}

/// Canonical host-side normalization for every pending/outbox ingress path.
/// Persistent backends store signed BIGINT values, so all implementations use
/// the same bounded representation before idempotency comparisons.
#[cfg(feature = "durable")]
pub(crate) fn normalize_pending_millis(mut input: PendingInput) -> PendingInput {
    input.available_at_ms = input.available_at_ms.map(crate::clock::normalize_millis);
    input
}

#[cfg(feature = "durable")]
pub(crate) fn installed_worker_credential_capabilities(
    worker: &crate::WorkerSnapshot,
) -> Result<awaken_runtime_contract::CredentialRealizationCapabilities, DispatchError> {
    awaken_run_ingress_contract::worker_credential_realization_capabilities(worker).map_err(
        |error| DispatchError::Rejected(format!("Worker capability evidence is invalid: {error}")),
    )
}

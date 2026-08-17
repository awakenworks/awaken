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
    RetryExhausted { max_attempts: u64 },
}

#[cfg(feature = "durable")]
impl ExactClaimMode {
    pub(crate) fn bypasses_execution_admission(self) -> bool {
        !matches!(self, Self::Runnable)
    }

    pub(crate) fn retry_limit(self) -> Option<u64> {
        match self {
            Self::RetryExhausted { max_attempts } => Some(max_attempts),
            Self::Runnable | Self::TerminalRecovery => None,
        }
    }
}

/// Decode SQL evidence once, then apply the storage-neutral retry policy. Query
/// filters are advisory; SQLite and PostgreSQL both call this after their
/// transactional exact-row read before advancing the claim epoch.
#[cfg(feature = "durable")]
pub(crate) fn retry_exhaustion_evidence_is_eligible(
    status: &str,
    lease_until: Option<i64>,
    attempt_count: i64,
    max_attempts: u64,
    now_ms: u64,
) -> Result<bool, DispatchError> {
    let phase = DispatchState::from_db(status)
        .ok_or_else(|| {
            DispatchError::Rejected(format!("unknown persisted dispatch state {status}"))
        })?
        .transition_phase();
    let lease_until = lease_until
        .map(crate::clock::millis_from_db)
        .transpose()
        .map_err(|error| DispatchError::Rejected(error.to_string()))?;
    let attempt_count = crate::durable_u64("dispatch attempt count", attempt_count)?;
    Ok(awaken_run_ingress_contract::retry_exhaustion_eligible(
        phase,
        lease_until,
        attempt_count,
        max_attempts,
        now_ms,
    ))
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

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use awaken_session_contract::SessionRealizationLease;
use awaken_worker_contract::{WorkerDirectory, WorkerIdentity};
use awaken_worker_transport_security::{VerifiedWorkerContext, verify_current_worker_identity};
use axum::http::StatusCode;

/// Closed result of the common registered-Worker/Session-effect admission.
/// Resource-specific Session, Workspace, command, and binding checks remain in
/// their owning adapters and run only after this shared boundary returns Exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionWorkerEffectAdmission {
    Exact,
    Expired,
    Foreign,
}

/// Temporal rule for the shared Worker-identity admission. Ordinary effects
/// are bounded by their asserted lease. A terminal generation may outlive that
/// timestamp, but this mode grants no Session authority: its adapter must still
/// obtain a current same-generation authorization from Control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionWorkerEffectTemporalRule {
    AssertedLeaseMustBeLive,
    TerminalGeneration,
}

impl SessionWorkerEffectAdmission {
    /// The one HTTP projection for Session Worker-effect rejection. Foreign
    /// identities disclose no lease state; an exact but expired effect is a
    /// recoverable generation conflict.
    pub(crate) const fn rejection_status(self) -> Option<StatusCode> {
        match self {
            Self::Exact => None,
            Self::Expired => Some(StatusCode::CONFLICT),
            Self::Foreign => Some(StatusCode::FORBIDDEN),
        }
    }
}

#[must_use]
const fn session_worker_effect_admission_from_facts(
    worker_is_current: bool,
    owner_matches: bool,
    incarnation_matches: bool,
    lease_is_live: bool,
    temporal_rule: SessionWorkerEffectTemporalRule,
) -> SessionWorkerEffectAdmission {
    if !worker_is_current || !owner_matches || !incarnation_matches {
        SessionWorkerEffectAdmission::Foreign
    } else if matches!(
        temporal_rule,
        SessionWorkerEffectTemporalRule::AssertedLeaseMustBeLive
    ) && !lease_is_live
    {
        SessionWorkerEffectAdmission::Expired
    } else {
        SessionWorkerEffectAdmission::Exact
    }
}

/// Verify the sole Worker-registry truth and the exact Session realization
/// generation carried by a Session Resource effect. This function deliberately
/// does not authorize a Resource operation; each adapter must still consult its
/// existing Session-root and Resource authority after an Exact result.
pub(crate) async fn verify_session_worker_effect(
    directory: &dyn WorkerDirectory,
    worker: &VerifiedWorkerContext,
    identity: &WorkerIdentity,
    lease: &SessionRealizationLease,
    now_unix_ms: u64,
    temporal_rule: SessionWorkerEffectTemporalRule,
) -> SessionWorkerEffectAdmission {
    let worker_is_current =
        verify_current_worker_identity(directory, worker, identity, now_unix_ms, false)
            .await
            .is_ok();
    session_worker_effect_admission_from_facts(
        worker_is_current,
        lease.owner == identity.worker_id,
        lease.runtime_incarnation == identity.lease_owner(),
        awaken_session_contract::realization_lease_is_live_at(
            lease.expires_at_unix_ms,
            now_unix_ms,
        ),
        temporal_rule,
    )
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Process-clock projection shared by every Resource Worker route. Pre-epoch
/// clocks fail closed as zero; unrepresentable far-future values saturate.
pub(crate) fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(duration_millis)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
}

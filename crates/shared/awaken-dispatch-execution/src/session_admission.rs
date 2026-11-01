// @formal surface obligation=session_execution.one_physical_attempt_per_thread
// @formal reason: Dispatch-to-Session activity admission port owned by its only caller, the Worker's
// @formal reason: reservation repair. It declares authority ownership; actual slot admission stays in
// @formal reason: the guarded store and the Coordinator implementation. The bounded model link is not
// @formal reason: a proof of arbitrary callback implementations.
//! The Session activity admission the Worker requests while repairing a
//! claimed Session reservation.
//!
//! The Worker is the only caller, so the port lives with it (ADR-0091 D4); the
//! Runtime Host implements it over the claim-fenced Coordinator transport.
//! Dispatch persists only its own twin of the answer,
//! `SessionRunReservationResolution`.

use async_trait::async_trait;
use awaken_dispatch_contract::{DispatchSettlementError, RunDispatch};
use awaken_session_execution_contract::{
    SessionRunActivityAdmission, SessionRunActivityAdmissionMode,
};
use awaken_worker_contract::RunClaim;

/// Exact Session activity admission used only while repairing a claimed
/// reservation. Non-Session terminal observers do not implement this role.
#[async_trait]
pub trait SessionRunAdmissionObserver: Send + Sync {
    /// Physical/projection preparation must run before acquiring reservation
    /// authority. Ordinary reservations retain their existing callback.
    async fn prepare_session_run_activity(
        &self,
        dispatch: &RunDispatch,
        _claim: &RunClaim,
        _mode: SessionRunActivityAdmissionMode,
    ) -> Result<Option<awaken_session_types::SessionRevision>, DispatchSettlementError> {
        if dispatch.identity_scope
            == awaken_dispatch_contract::DispatchIdentityScope::PreparedSessionCommand
        {
            Err(DispatchSettlementError(
                "prepared activity preparation is unavailable".into(),
            ))
        } else {
            Ok(None)
        }
    }

    async fn admit_session_run_activity(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        mode: SessionRunActivityAdmissionMode,
        preparation: Option<awaken_session_execution_contract::PreparedSessionRunActivity>,
    ) -> Result<SessionRunActivityAdmission, DispatchSettlementError>;
}

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
use awaken_dispatch_contract::{DispatchSettlementError, RunClaim, RunDispatch};
use awaken_session_execution_contract::{
    SessionRunActivityAdmission, SessionRunActivityAdmissionMode,
};

/// Exact Session activity admission used only while repairing a claimed
/// reservation. Non-Session terminal observers do not implement this role.
#[async_trait]
pub trait SessionRunAdmissionObserver: Send + Sync {
    async fn admit_session_run_activity(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        mode: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, DispatchSettlementError>;
}

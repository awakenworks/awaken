// Exact Dispatch-to-Session settlement observer port; included in dispatch
// to preserve its public namespace alongside the existing queue contract.

/// Failure at a committed Run boundary that must complete before delivery
/// state can be removed or returned to Awaiting.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("dispatch settlement observer failed: {0}")]
pub struct DispatchSettlementError(pub String);

/// Fallible pre-settlement effect over trusted dispatch/claim coordinates and
/// committed Run truth. An error retains the dispatch row as retry evidence.
#[async_trait]
pub trait DispatchSettlementObserver: Send + Sync {
    /// Whether this observer's transport owns the authoritative Dispatch guard.
    /// This is immutable topology, not an optional effect.
    fn owns_dispatch_fence(&self) -> bool {
        false
    }

    /// Whether this observer's transport owns the authoritative claim guard
    /// for Session admission and quiescent settlement. Local observers return
    /// false so the Worker obtains the colocated guard; authenticated remote
    /// adapters return true because the guard exists at the Coordinator endpoint.
    /// Reservation CAS retains the guard. Settlement callbacks may re-enter
    /// Dispatch, so that boundary releases it after checking the occupied slot.
    /// Repair the exact Session activity admission before an expired Reserved
    /// intent is published to ordinary Pending. Implementations must derive a
    /// stable operation from the canonical Run identity.
    async fn before_settle(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        committed_state: &RunState,
        cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError>;

    /// Observe the successful Dispatch CAS. Non-Session observers need no
    /// second effect; the Session adapter overrides this to publish the typed
    /// receipt that gates User Event processing and final activity closure.
    async fn after_settle(
        &self,
        _dispatch: &RunDispatch,
        _claim: &RunClaim,
        _committed_state: &RunState,
        _cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError> {
        Ok(())
    }
}

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

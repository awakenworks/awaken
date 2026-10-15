// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Static owner: claim-fenced Session realization port. Dynamic path: a Worker may resume
// @formal reason: Session realization state only through the exact claimed dispatch snapshot and epoch.
// @formal reason: Evidence: worker.remote_execution.remote_protocol and claimed-worker conformance
// @formal reason: tests; this is a product-requirement classification, not a direct proof claim.
//! Claim-fenced Session realization over the authenticated Worker transport.
//!
//! The Session owns this role; the dispatch claim only carries it (ADR-0088
//! D3). The Coordinator transport validates the claim before invoking the
//! claim-free Session application APIs, so `RunClaim` never enters the durable
//! Session aggregate.

use awaken_session_application_contract::SessionWorkAuthorityReceipt;
use awaken_worker_contract::RunClaim;

use crate::{
    SessionRealizationControl, SessionRealizationControlDisposition,
    SessionRealizationControlFailure, SessionRealizationDirective,
};

/// Failure crossing the authenticated claim-fenced Session-control boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("claimed Session control failed: {0}")]
pub struct ClaimedSessionControlError(SessionRealizationControlFailure);

impl ClaimedSessionControlError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(SessionRealizationControlFailure::Unavailable(
            message.into(),
        ))
    }

    #[must_use]
    pub const fn disposition(&self) -> SessionRealizationControlDisposition {
        self.0.disposition()
    }

    #[must_use]
    pub const fn is_not_ready(&self) -> bool {
        matches!(
            self.disposition(),
            SessionRealizationControlDisposition::NotReady
        )
    }
}

impl From<SessionRealizationControlFailure> for ClaimedSessionControlError {
    fn from(error: SessionRealizationControlFailure) -> Self {
        Self(error)
    }
}

/// Worker-side outbound port over the authenticated Coordinator transport.
#[async_trait::async_trait]
pub trait ClaimedSessionRealizationControl: SessionRealizationControl + Send + Sync {
    /// Resume committed projection state. `continuation` is an assertion of
    /// this same Run's still-live local realization, never authority by itself.
    /// The authenticated adapter must renew its exact fence and reject a
    /// different returned generation; absence requests ordinary reassignment.
    async fn resume_frozen(
        &self,
        claim: &RunClaim,
        session_id: &str,
        session_work_authority: Option<&SessionWorkAuthorityReceipt>,
        continuation: Option<&crate::SessionRealizationLease>,
    ) -> Result<Option<SessionRealizationDirective>, ClaimedSessionControlError>;
}

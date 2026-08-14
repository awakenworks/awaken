//! Claim-fenced bridge from Worker execution to the Session application.
//!
//! The bridge owns only the dispatch claim envelope. The Coordinator transport
//! validates that envelope before invoking the claim-free Session application
//! APIs, so `RunClaim` never enters the durable Session aggregate.

use crate::RunClaim;
use awaken_session_contract::{SessionRealizationControl, SessionRealizationDirective};

/// Failure crossing the authenticated claim-fenced Session-control boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("claimed Session control failed: {0}")]
pub struct ClaimedSessionControlError(awaken_session_contract::SessionRealizationControlFailure);

impl ClaimedSessionControlError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(awaken_session_contract::SessionRealizationControlFailure::Unavailable(message.into()))
    }

    #[must_use]
    pub const fn is_not_ready(&self) -> bool {
        matches!(
            self.0,
            awaken_session_contract::SessionRealizationControlFailure::NotReady
        )
    }
}

impl From<awaken_session_contract::SessionRealizationControlFailure>
    for ClaimedSessionControlError
{
    fn from(error: awaken_session_contract::SessionRealizationControlFailure) -> Self {
        Self(error)
    }
}

/// Worker-side outbound port over the authenticated Coordinator transport.
#[async_trait::async_trait]
pub trait ClaimedSessionControl: SessionRealizationControl + Send + Sync {
    async fn resume_frozen(
        &self,
        claim: &RunClaim,
        session_id: &str,
    ) -> Result<Option<SessionRealizationDirective>, ClaimedSessionControlError>;
}

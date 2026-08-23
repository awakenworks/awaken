//! Claim-fenced bridge from Worker execution to the Session application.
//!
//! The bridge owns only the dispatch claim envelope. The Coordinator transport
//! validates that envelope before invoking the claim-free Session application
//! APIs, so `RunClaim` never enters the durable Session aggregate.

use crate::RunClaim;
use awaken_session_contract::{
    SessionAgentBoundaryCommand, SessionAgentMessageCommand, SessionAgentMessageReceipt,
    SessionAgentRosterEntry, SessionRealizationControl, SessionRealizationDirective,
    SessionRunActivityAdmission, SessionRunActivityAdmissionMode,
};

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
    pub const fn disposition(
        &self,
    ) -> awaken_session_contract::SessionRealizationControlDisposition {
        self.0.disposition()
    }

    #[must_use]
    pub const fn is_not_ready(&self) -> bool {
        matches!(
            self.disposition(),
            awaken_session_contract::SessionRealizationControlDisposition::NotReady
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

    async fn list_session_agents(
        &self,
        _claim: &RunClaim,
        _session_id: &str,
    ) -> Result<Vec<SessionAgentRosterEntry>, ClaimedSessionControlError> {
        Err(ClaimedSessionControlError::new(
            "claimed Session Agent coordination is unsupported",
        ))
    }

    async fn admit_session_model_request(
        &self,
        _claim: &RunClaim,
        _session_id: &str,
        _thread_id: &awaken_agent_contract::agent::thread::Id,
        _run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<bool, ClaimedSessionControlError> {
        Err(ClaimedSessionControlError::new(
            "claimed Session model-request admission is unsupported",
        ))
    }

    async fn admit_session_run_activity(
        &self,
        _claim: &RunClaim,
        _session_id: &str,
        _agent_id: &str,
        _run_id: &awaken_agent_contract::agent::run::Id,
        _mode: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, ClaimedSessionControlError> {
        Err(ClaimedSessionControlError::new(
            "claimed Session Run activity admission is unsupported",
        ))
    }

    async fn send_session_agent_message(
        &self,
        _claim: &RunClaim,
        _command: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, ClaimedSessionControlError> {
        Err(ClaimedSessionControlError::new(
            "claimed Session Agent coordination is unsupported",
        ))
    }

    async fn settle_session_agent_boundary(
        &self,
        _claim: &RunClaim,
        _command: SessionAgentBoundaryCommand,
    ) -> Result<(), ClaimedSessionControlError> {
        Err(ClaimedSessionControlError::new(
            "claimed Session Agent settlement is unsupported",
        ))
    }
}

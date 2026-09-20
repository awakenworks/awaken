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
pub trait ClaimedSessionRealizationControl: SessionRealizationControl + Send + Sync {
    /// Resume committed projection state. `continuation` is an assertion of
    /// this same Run's still-live local realization, never authority by itself.
    /// The authenticated adapter must renew its exact fence and reject a
    /// different returned generation; absence requests ordinary reassignment.
    async fn resume_frozen(
        &self,
        claim: &RunClaim,
        session_id: &str,
        session_work_authority: Option<
            &awaken_session_contract::work_queue::SessionWorkAuthorityReceipt,
        >,
        continuation: Option<&awaken_session_contract::SessionRealizationLease>,
    ) -> Result<Option<SessionRealizationDirective>, ClaimedSessionControlError>;
}

/// Claim-fenced Session Agent coordination over the authenticated Worker
/// transport. It is independent from realization resume so a topology that
/// only recovers projections cannot inherit runtime failures for coordination.
#[async_trait::async_trait]
pub trait ClaimedSessionCoordinationControl: Send + Sync {
    async fn list_session_agents(
        &self,
        claim: &RunClaim,
        session_id: &str,
    ) -> Result<Vec<SessionAgentRosterEntry>, ClaimedSessionControlError>;

    async fn admit_session_model_request(
        &self,
        claim: &RunClaim,
        session_id: &str,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<bool, ClaimedSessionControlError>;

    async fn admit_session_run_activity(
        &self,
        claim: &RunClaim,
        session_id: &str,
        agent_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
        mode: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, ClaimedSessionControlError>;

    async fn send_session_agent_message(
        &self,
        claim: &RunClaim,
        command: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, ClaimedSessionControlError>;

    async fn settle_session_agent_boundary(
        &self,
        claim: &RunClaim,
        command: SessionAgentBoundaryCommand,
    ) -> Result<(), ClaimedSessionControlError>;
}

pub trait ClaimedSessionControl:
    ClaimedSessionRealizationControl + ClaimedSessionCoordinationControl
{
}

impl<T> ClaimedSessionControl for T where
    T: ClaimedSessionRealizationControl + ClaimedSessionCoordinationControl + ?Sized
{
}

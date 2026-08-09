//! Claim-envelope-free application contribution command owned by the Session context.
//!
//! Worker identity and Run claim authorization are transport concerns. The
//! authenticated Control adapter strips those values before invoking this port;
//! only the complete, secret-free application input enters the Session service.

use async_trait::async_trait;

use crate::{
    ApplicationContributionOutcome, ApplicationSessionInput, ResolvedSessionResources,
    SessionBaseline, SessionMcpAttachment, SessionRevision,
};

/// One complete application input for a Session that is still preparing.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ApplicationSessionContribution {
    pub session_id: String,
    pub application_fingerprint: String,
    pub input: ApplicationSessionInput,
}

/// Exact durable projection returned after the creation intent is consumed.
///
/// A Worker may cache this value only as realization input. It is reconstructed
/// from the Control-owned aggregate on replay and is never a second desired-state
/// authority.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrozenSessionProjection {
    pub workspace_id: String,
    pub revision: SessionRevision,
    pub baseline: SessionBaseline,
    /// Exact generation owned by `SessionResourceState`. This is distinct from
    /// the Session root revision above and must survive Control-to-Worker
    /// realization so a new Run never reconstructs its resource envelope at the
    /// legacy generation zero.
    #[serde(default)]
    pub resource_revision: u64,
    pub resources: ResolvedSessionResources,
    #[serde(default)]
    pub toolsets: Vec<awaken_agent_contract::ToolsetPolicy>,
    pub mcp: Vec<SessionMcpAttachment>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ApplicationSessionContributionReceipt {
    pub outcome: ApplicationContributionOutcome,
    pub projection: FrozenSessionProjection,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ApplicationSessionContributionFailure {
    #[error("Session was not found")]
    NotFound,
    #[error("Session does not require an application contribution")]
    NotRequired,
    #[error("Session already accepted a different application contribution")]
    Conflict,
    #[error("application contribution is invalid: {0}")]
    Invalid(String),
    #[error("application contribution could not be committed: {0}")]
    Unavailable(String),
}

/// Control-owned application service port. Claim fencing must be completed and
/// held by the caller for the duration of this command.
#[async_trait]
pub trait ApplicationSessionContributionApi: Send + Sync {
    async fn contribute_application(
        &self,
        contribution: ApplicationSessionContribution,
    ) -> Result<ApplicationSessionContributionReceipt, ApplicationSessionContributionFailure>;
}

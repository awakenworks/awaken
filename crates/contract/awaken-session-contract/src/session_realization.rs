//! Control-owned Session realization phase protocol (ADR-0066 D5).
//!
//! The Session aggregate owns durable desired and lifecycle state. A Runtime
//! projection owns only staged/live effects. These commands let local and remote
//! topology adapters drive the same root-CAS transitions without giving a Worker
//! repository access or creating a second MCP desired-state registry.

use async_trait::async_trait;

use crate::{
    FrozenSessionProjection, McpGenerationRef, McpRealizationReceipt, SessionRealizationLease,
    StageMcpAttachment,
};

/// Opaque Runtime assignment selected outside the Session domain. Worker and
/// local-process identities are mapped to these strings at the authenticated
/// application edge; the aggregate never imports their protocol vocabulary.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationTarget {
    pub owner: String,
    pub runtime_incarnation: String,
    pub lease_expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum SessionRealizationAction {
    /// Realize the frozen Environment/Resources and stage every exact MCP
    /// request. `prepare_session` is the exact Environment/Resource work bit;
    /// MCP-only hot updates must not recreate an already-live environment.
    Stage {
        prepare_session: bool,
        #[serde(default)]
        mcp_stages: Vec<StageMcpAttachment>,
    },
    /// Durable activation has committed. Publish and drain only these exact
    /// generations before acknowledging the effect back to Control.
    Publish {
        #[serde(default)]
        publish: Vec<McpGenerationRef>,
        #[serde(default)]
        drain: Vec<McpGenerationRef>,
    },
    /// Every required projection acknowledgement is durable.
    Complete,
}

/// Secret-free next action returned after every successful Control phase.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationDirective {
    pub projection: FrozenSessionProjection,
    pub lease: SessionRealizationLease,
    pub action: SessionRealizationAction,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BeginSessionRealization {
    pub session_id: String,
    pub target: SessionRealizationTarget,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ActivateSessionRealization {
    pub session_id: String,
    pub lease: SessionRealizationLease,
    #[serde(default)]
    pub mcp_receipts: Vec<McpRealizationReceipt>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AcknowledgeSessionRealization {
    pub session_id: String,
    pub lease: SessionRealizationLease,
    #[serde(default)]
    pub published: Vec<McpGenerationRef>,
    #[serde(default)]
    pub drained: Vec<McpGenerationRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailSessionRealization {
    pub session_id: String,
    pub lease: SessionRealizationLease,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionRealizationControlFailure {
    #[error("Session was not found")]
    NotFound,
    #[error("Session is not ready for this realization phase")]
    NotReady,
    #[error("Session realization ownership is stale")]
    StaleOwnership,
    #[error("Session changed concurrently")]
    Conflict,
    #[error("Session realization command is invalid: {0}")]
    Invalid(String),
    #[error("Session realization service is unavailable: {0}")]
    Unavailable(String),
}

/// Driving port for the Control-owned realization state machine. It performs no
/// Runtime I/O; topology adapters execute the returned action and submit exact
/// receipts/acknowledgements to the next phase.
#[async_trait]
pub trait SessionRealizationControl: Send + Sync {
    async fn begin_session_realization(
        &self,
        command: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure>;

    async fn activate_session_realization(
        &self,
        command: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure>;

    async fn acknowledge_session_realization(
        &self,
        command: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure>;

    async fn fail_session_realization(
        &self,
        command: FailSessionRealization,
    ) -> Result<(), SessionRealizationControlFailure>;
}

/// One application service is installed behind Worker transport. This
/// supertrait prevents contribution and realization from being accidentally
/// wired to different Session authorities.
pub trait ApplicationSessionControl:
    crate::ApplicationSessionContributionPort + SessionRealizationControl
{
}

impl<T> ApplicationSessionControl for T where
    T: crate::ApplicationSessionContributionPort + SessionRealizationControl
{
}

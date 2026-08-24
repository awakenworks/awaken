//! Durable Deployment records and the one scheduled-occurrence claim port.
//!
//! The Managed protocol adapter owns wire projection. This inward contract keeps
//! storage independent of that wire vocabulary while making restart recovery and
//! multi-replica schedule claims explicit.

use async_trait::async_trait;
mod model;
mod schedule;

/// Compatibility name for the one cross-aggregate lifecycle fact contract.
pub use awaken_session_contract::ManagedLifecycleFact as DeploymentLifecycleFact;
pub use model::{
    AgentSelector, CreateDeploymentCommand, DeploymentAgent, DeploymentLaunch,
    DeploymentLaunchOutcome, DeploymentOutcomeRubric, DeploymentPauseError, DeploymentPauseReason,
    DeploymentRecord, DeploymentRepositoryCheckout, DeploymentResource, DeploymentRunFailure,
    DeploymentRunRecord, DeploymentRunView, DeploymentSchedule, DeploymentSeedEvent,
    DeploymentStatus, DeploymentTrigger, DeploymentView, FieldUpdate, MetadataUpdate,
    UpdateDeploymentCommand,
};
pub use schedule::Cron;

/// Largest revision representable by every supported durable SQL adapter.
pub const MAX_DEPLOYMENT_REVISION: u64 = i64::MAX as u64;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeploymentRepositoryError {
    #[error("Deployment repository failure: {0}")]
    Storage(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentWriteOutcome {
    Applied,
    Conflict,
    ScheduledCapacityReached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduledRunClaimOutcome {
    Claimed,
    AlreadyClaimed,
    StaleDeployment,
}

#[async_trait]
pub trait DeploymentRepository: Send + Sync {
    async fn deployments(&self) -> Result<Vec<DeploymentView>, DeploymentRepositoryError>;

    async fn deployment_runs(&self) -> Result<Vec<DeploymentRunView>, DeploymentRepositoryError>;

    /// Create at revision zero (`expected_revision=None`) or compare-and-swap
    /// one Deployment to the exact successor revision. Capacity admission and
    /// the lifecycle fact commit in the same transaction.
    async fn write_deployment(
        &self,
        deployment: DeploymentView,
        expected_revision: Option<u64>,
        scheduled_limit: usize,
        lifecycle: Option<DeploymentLifecycleFact>,
    ) -> Result<DeploymentWriteOutcome, DeploymentRepositoryError>;

    async fn upsert_deployment_run(
        &self,
        run: DeploymentRunView,
        lifecycle: Option<DeploymentLifecycleFact>,
    ) -> Result<(), DeploymentRepositoryError>;

    /// Atomically claim one exact `(Deployment, scheduled instant)`, advance to
    /// the exact successor revision, and persist its first DeploymentRun.
    async fn claim_scheduled_run(
        &self,
        claim_id: &str,
        expected_deployment_revision: u64,
        deployment: DeploymentView,
        run: DeploymentRunView,
        lifecycle: DeploymentLifecycleFact,
    ) -> Result<ScheduledRunClaimOutcome, DeploymentRepositoryError>;
}

/// Coordinator lifecycle command consumed after an Agent is archived.
#[async_trait]
pub trait AgentArchiveCascade: Send + Sync {
    async fn archive_agent_dependents(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<(), String>;
}

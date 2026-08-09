//! Durable Deployment records and the one scheduled-occurrence claim port.
//!
//! The Managed protocol adapter owns wire projection. This inward contract keeps
//! storage independent of that wire vocabulary while making restart recovery and
//! multi-replica schedule claims explicit.

use async_trait::async_trait;
use awaken_session_contract::ManagedLifecycleFact;

mod schedule;

pub use schedule::Cron;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentRecord {
    pub deployment_id: String,
    pub workspace_id: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentRunRecord {
    pub run_id: String,
    pub deployment_id: String,
    pub workspace_id: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeploymentRepositoryError {
    #[error("Deployment repository failure: {0}")]
    Storage(String),
}

#[async_trait]
pub trait DeploymentRepository: Send + Sync {
    async fn deployments(&self) -> Result<Vec<DeploymentRecord>, DeploymentRepositoryError>;

    async fn deployment_runs(&self) -> Result<Vec<DeploymentRunRecord>, DeploymentRepositoryError>;

    async fn upsert_deployment(
        &self,
        record: DeploymentRecord,
        lifecycle: Option<ManagedLifecycleFact>,
    ) -> Result<(), DeploymentRepositoryError>;

    async fn upsert_deployment_run(
        &self,
        record: DeploymentRunRecord,
        lifecycle: Option<ManagedLifecycleFact>,
    ) -> Result<(), DeploymentRepositoryError>;

    /// Atomically claim one exact `(Deployment, scheduled instant)` and persist
    /// its first DeploymentRun. `false` is an idempotent loss to another replica.
    async fn claim_scheduled_run(
        &self,
        claim_id: &str,
        deployment: DeploymentRecord,
        run: DeploymentRunRecord,
        lifecycle: ManagedLifecycleFact,
    ) -> Result<bool, DeploymentRepositoryError>;
}

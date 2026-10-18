// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Original frozen projection and MCP effects use the canonical realization driver and existing installed Sessions.
//! Realizes an already-authorized frozen Session projection. Dispatch and Run execution supply the same projection effect role.
use crate::{
    realization_error::RealizationError, realization_executor::RealizationExecutor,
    session_realizations::SessionRealizations,
};
use awaken_dispatch_contract::RunClaim;
use awaken_runtime_contract::ExecutableAgentSnapshot;
use awaken_session_configuration_contract::resource::SessionRealizationLease;
use awaken_session_realization_contract::{
    FrozenSessionProjection, ObservedSessionRealizationLease,
};
use awaken_session_execution_contract::McpAttachmentRealizer;
/// Installs frozen inputs through the existing Runtime projection boundary.
#[async_trait::async_trait]
pub trait SessionProjectionExecution: Send + Sync {
    fn sessions(&self) -> SessionRealizations;
    fn realization_executor(&self) -> &RealizationExecutor;
    async fn ensure_session_substrate(
        &self,
        thread: &str,
        publication: Option<&ExecutableAgentSnapshot>,
    ) -> Result<(), RealizationError>;
    async fn install_frozen_projection(
        &self,
        session: &str,
        projection: FrozenSessionProjection,
        claim: Option<&RunClaim>,
        synchronize_resources: bool,
        lease: Option<ObservedSessionRealizationLease>,
    ) -> Result<(), RealizationError>;
    fn invalidate_realization_lease(
        &self,
        session: &str,
        lease: &SessionRealizationLease,
    ) -> Result<(), RealizationError>;
    fn mcp_effects(&self) -> &dyn McpAttachmentRealizer;
}

pub mod projection_synchronizer;

pub use projection_synchronizer::{WorkerMcpEffects, WorkerProjectionSynchronizer};

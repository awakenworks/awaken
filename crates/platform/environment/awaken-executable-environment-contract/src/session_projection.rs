//! Environment-owned executable projection queries for Session admission.

use crate::{EnvironmentImageBuildError, ExecutableEnvironmentRegistrationError};
use awaken_agent_contract::McpTarget;
use awaken_environment_contract::EnvItem;

/// Exact executable Environment projection resolved for one Session creation.
pub struct ResolvedSessionEnvironment<Snapshot> {
    pub snapshot: Snapshot,
}

/// Coordinator projection consumed by Session compilation. Environment HTTP
/// routes may use the same concrete adapter, but their wire types stay outside
/// this service contract.
#[async_trait::async_trait]
pub trait SessionEnvironmentProjectionSource: Send + Sync {
    type Snapshot: Send + Sync;

    async fn get(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError>;

    async fn resolve_current_for_session(
        &self,
        environment_id: &str,
        runtime: Option<&str>,
        mcp_targets: &[McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment<Self::Snapshot>>, EnvironmentImageBuildError>;

    async fn resolve_exact_for_session(
        &self,
        environment_id: &str,
        revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment<Self::Snapshot>>, EnvironmentImageBuildError>;
}

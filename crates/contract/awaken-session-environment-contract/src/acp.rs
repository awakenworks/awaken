// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: This frozen Session execution port carries the selected backend, permission,
// @formal reason: MCP and process environment into one factory; classification is not a proof claim.
//! Frozen per-Session facts at the ACP execution edge.
//!
//! The Session Host owns selection, permission, MCP route staging, and Run
//! commits. The Worker composition implements this port using its one ACP
//! source and process owner; the contract owns no launch or catalog logic.

use std::sync::Arc;

use crate::tool_export::AttachedToolSet;
use awaken_acp_contract::{AcpAdapterPolicy, SessionMcpServer};
use awaken_runtime_contract::execution::RunAttemptExecutor;
use awaken_runtime_contract::permission::{PermissionGrantObserver, ToolPermissionPolicy};
use awaken_runtime_contract::resolved::{Backend, ProviderServerTool};
use awaken_runtime_contract::resume::OwnedToolContinuation;

use crate::AgentProcessEnvironment;

/// A single frozen root or child ACP execution request. It never authorizes a
/// backend, creates a Session, or commits a Run.
pub struct AcpSessionExecution {
    pub environment: Arc<dyn AgentProcessEnvironment>,
    pub backend: Backend,
    pub permission: Arc<dyn ToolPermissionPolicy>,
    pub mcp_servers: Vec<SessionMcpServer>,
    pub attached_tools: Option<Arc<AttachedToolSet>>,
    pub grant_observer: Option<Arc<dyn PermissionGrantObserver>>,
    pub continuation: Option<Arc<dyn OwnedToolContinuation>>,
}

/// The one process-installed ACP executor construction authority. Each call
/// consumes only Host-frozen facts; the implementation owns the concrete
/// launch, channel source, and recovery adapter.
pub trait AcpSessionExecutorFactory: Send + Sync {
    fn create(&self, request: AcpSessionExecution) -> Result<Arc<dyn RunAttemptExecutor>, String>;

    /// The same canonical catalog that its launch source uses for exact CLI,
    /// MCP delivery, and provider-tool decisions.
    fn adapter_policy(&self) -> &dyn AcpAdapterPolicy<ProviderServerTool>;

    /// Report the same installed launch source's exact credential capability.
    fn credential_realization_capabilities(
        &self,
        backend: &Backend,
    ) -> Result<awaken_credential_contract::CredentialRealizationCapabilities, String>;
}

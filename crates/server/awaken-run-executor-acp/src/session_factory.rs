// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Static ACP embeddings implement the same one Session factory port;
// @formal reason: classification records the authorization boundary, not a proof claim.
//! Existing static ACP executor as a fixture and embedding implementation of
//! the same per-Session factory port used by bound Worker composition.

use std::sync::Arc;

use awaken_runtime_contract::execution::RunAttemptExecutor;
use awaken_runtime_contract::resolved::Backend;
use awaken_session_environment_contract::{AcpSessionExecution, AcpSessionExecutorFactory};

use crate::AcpRunExecutor;

impl AcpSessionExecutorFactory for AcpRunExecutor {
    fn adapter_policy(
        &self,
    ) -> &dyn awaken_acp_contract::AcpAdapterPolicy<
        awaken_runtime_contract::resolved::ProviderServerTool,
    > {
        crate::acp_adapter_policy()
    }

    fn create(&self, request: AcpSessionExecution) -> Result<Arc<dyn RunAttemptExecutor>, String> {
        if request.attached_tools.is_some() {
            return Err("attached MCP tools require the bound Environment ACP source".into());
        }
        let executor = self
            .for_session_servers_with_grant_observer(
                request.permission,
                &request.mcp_servers,
                request.grant_observer,
            )
            .with_owned_tool_continuation(request.continuation);
        Ok(Arc::new(executor))
    }

    fn credential_realization_capabilities(
        &self,
        _backend: &Backend,
    ) -> Result<awaken_runtime_contract::CredentialRealizationCapabilities, String> {
        Ok(Default::default())
    }
}

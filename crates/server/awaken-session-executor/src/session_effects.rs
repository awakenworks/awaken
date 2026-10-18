// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The configured Session MCP realizer and the ordinary Run MCP role over the same Session
// @formal reason: assembly retain the same lifecycle and MCP authorities; neither holds a back-reference.
//! Run-side effects reached by claimed execution: the Memory terminal
//! observer of a guarded dispatch, the Session MCP realizer the composition
//! selected, and the ordinary Run MCP role (SDK Direct admission).
use awaken_session_types::RunError;
use std::sync::Arc;

impl crate::RunAssembly {
    /// Build the terminal observer from the exact guarded dispatch. The logical
    /// Thread owns transcript/extraction identity; `commit` may still point at
    /// its parent Session's physical partition.
    pub(crate) async fn dispatched_memory_terminal_observer(
        &self,
        dispatch: &awaken_dispatch_contract::RunDispatch,
        commit: Arc<crate::store::HostCommit>,
    ) -> Result<
        Option<Arc<dyn awaken_runtime_contract::terminal::RunTerminalObserver>>,
        crate::HostError,
    > {
        let manifest =
            awaken_dispatch_execution::session_envelope::decode_dispatched_resource_manifest(
                dispatch,
            )
            .map_err(crate::HostError::internal)?;
        let workspace = dispatch
            .execution_scope
            .as_ref()
            .map_or_else(|| self.local_workspace(), |scope| scope.0.0.as_str());
        let frozen_publications = crate::agent_catalog::RUN_PUBLICATIONS
            .exact_source(
                &dispatch.activation.snapshot,
                &dispatch.agent_publications,
                workspace,
            )
            .map_err(crate::HostError::internal)?;
        self.memory_terminal_observer(
            &dispatch.session_thread_id().0,
            &dispatch.activation.snapshot,
            dispatch.activation.effective_model_ref(),
            manifest.as_ref(),
            frozen_publications.as_ref(),
            commit,
        )
        .await
    }

    /// The one Session MCP attachment realizer: the realizer the composition
    /// selected for durable Worker commands, else this assembly's own.
    pub(crate) fn session_mcp_realizer(
        &self,
    ) -> &dyn awaken_session_execution_contract::McpAttachmentRealizer {
        match &self.session_execution.mcp_realizer {
            Some(realizer) => realizer.as_ref(),
            None => self,
        }
    }
}

/// Ordinary Run compatibility effects borrow the same configured MCP owner.
/// Only this role selects the SDK's Direct admission; it never installs a root.
#[derive(Clone)]
pub(crate) struct RunMcpEffects {
    assembly: Arc<crate::RunAssembly>,
}

impl RunMcpEffects {
    pub(crate) fn new(assembly: Arc<crate::RunAssembly>) -> Self {
        Self { assembly }
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for RunMcpEffects {
    async fn stage_mcp_attachment(
        &self,
        request: crate::session_mcp::StageMcpAttachment,
    ) -> Result<crate::session_mcp::McpRealizationReceipt, RunError> {
        let assembly = self.assembly.as_ref();
        match &assembly.session_execution.mcp_realizer {
            Some(realizer) => realizer.stage_mcp_attachment(request).await,
            None => {
                assembly
                    .mcp
                    .stage_attachment(
                        &assembly.installed_sessions,
                        assembly,
                        assembly.mcp_credential_materialization(),
                        request,
                    )
                    .await
            }
        }
    }
    async fn publish_mcp_generation(
        &self,
        generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.assembly
            .session_mcp_realizer()
            .publish_mcp_generation(generation)
            .await
    }
    async fn drain_mcp_generation(
        &self,
        generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.assembly
            .session_mcp_realizer()
            .drain_mcp_generation(generation)
            .await
    }
}

impl awaken_dispatch_execution::session_execution::RunMcpAttachments for RunMcpEffects {
    fn active_stages(
        &self,
        session: &str,
    ) -> Result<Vec<crate::session_mcp::StageMcpAttachment>, RunError> {
        Ok(self
            .assembly
            .active_mcp_projections(session)
            .into_iter()
            .map(|projection| projection.request)
            .collect())
    }
}

#[cfg(test)]
mod tests;

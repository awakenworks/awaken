// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Constructor-selected effects use the original realizer and canonical MCP authority. Configured forwarding retains no unrelated Session owner; local Direct execution keeps its required staging context.
//! Constructor-selected MCP execution effects and local generation projections.
use awaken_session_types::RunError;
use std::sync::Arc;

impl crate::RunAssembly {
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

/// The exact constructor-selected realizer and a weak view of local projections.
/// A configured remote realizer owns no unrelated local Session execution state.
#[derive(Clone)]
pub(crate) struct RunMcpEffects {
    realizer: Arc<dyn awaken_session_execution_contract::McpAttachmentRealizer>,
    projections: std::sync::Weak<crate::RunAssembly>,
}
impl RunMcpEffects {
    pub(crate) fn new(assembly: Arc<crate::RunAssembly>) -> Self {
        let realizer = assembly
            .session_execution
            .mcp_realizer
            .clone()
            .unwrap_or_else(|| {
                Arc::new(awaken_session_mcp::DirectMcpAttachments::new(
                    assembly.mcp.clone(),
                    assembly.installed_sessions.clone(),
                    assembly.clone(),
                    crate::RunAssembly::mcp_credential_materialization,
                ))
            });
        Self {
            realizer,
            projections: Arc::downgrade(&assembly),
        }
    }
}
#[async_trait::async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for RunMcpEffects {
    async fn stage_mcp_attachment(
        &self,
        request: crate::session_mcp::StageMcpAttachment,
    ) -> Result<crate::session_mcp::McpRealizationReceipt, RunError> {
        self.realizer.stage_mcp_attachment(request).await
    }
    async fn publish_mcp_generation(
        &self,
        generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.realizer.publish_mcp_generation(generation).await
    }
    async fn drain_mcp_generation(
        &self,
        generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.realizer.drain_mcp_generation(generation).await
    }
}
impl awaken_dispatch_execution::session_execution::RunMcpAttachments for RunMcpEffects {
    fn active_stages(
        &self,
        session: &str,
    ) -> Result<Vec<crate::session_mcp::StageMcpAttachment>, RunError> {
        let assembly = self.projections.upgrade().ok_or_else(|| {
            RunError::unavailable_classified(
                "mcp_projection_owner_released",
                "the original MCP projection owner was released",
            )
        })?;
        Ok(assembly
            .active_mcp_projections(session)
            .map_err(crate::session_execution_error::to_run_error)?
            .into_iter()
            .map(|projection| projection.request)
            .collect())
    }
}

#[cfg(test)]
mod tests;

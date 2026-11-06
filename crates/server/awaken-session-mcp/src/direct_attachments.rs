// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: The SDK Direct attachment role retains the original MCP relay, canonical states and staging context, and borrows its selected credential capabilities per call. The existing lifecycle owns admission, publication and drain.
//! Owned SDK Direct attachment effects over the selected MCP lifecycle inputs.
use crate::{McpCredentialMaterialization, McpSessions, McpStagingSession, SessionMcpStates};
use awaken_runtime_contract::runs::RunError;
use awaken_session_configuration_contract::mcp::{
    McpGenerationRef, McpRealizationReceipt, StageMcpAttachment,
};
use awaken_session_execution_contract::McpAttachmentRealizer;
use std::sync::Arc;

/// SDK Direct admission, distinct from a Worker command assigned to a Session.
/// The original staging context stays alive until this execution role is dropped.
pub struct DirectMcpAttachments<S, C> {
    mcp: McpSessions,
    states: Arc<S>,
    context: Arc<C>,
    credentials: for<'a> fn(&'a C) -> McpCredentialMaterialization<'a>,
}
impl<S, C> DirectMcpAttachments<S, C> {
    #[must_use]
    pub fn new(
        mcp: McpSessions,
        states: Arc<S>,
        context: Arc<C>,
        credentials: for<'a> fn(&'a C) -> McpCredentialMaterialization<'a>,
    ) -> Self {
        Self {
            mcp,
            states,
            context,
            credentials,
        }
    }
}
#[async_trait::async_trait]
impl<S: SessionMcpStates + 'static, C: McpStagingSession + 'static> McpAttachmentRealizer
    for DirectMcpAttachments<S, C>
{
    async fn stage_mcp_attachment(
        &self,
        request: StageMcpAttachment,
    ) -> Result<McpRealizationReceipt, RunError> {
        self.mcp
            .stage_attachment(
                &self.states,
                self.context.clone(),
                self.credentials,
                request,
            )
            .await
    }
    async fn publish_mcp_generation(&self, generation: McpGenerationRef) -> Result<(), RunError> {
        self.mcp
            .publish_attachment(self.states.as_ref(), self.context.as_ref(), generation)
            .await
    }
    async fn drain_mcp_generation(&self, generation: McpGenerationRef) -> Result<(), RunError> {
        self.mcp
            .drain_attachment(self.states.as_ref(), generation)
            .await
    }
}

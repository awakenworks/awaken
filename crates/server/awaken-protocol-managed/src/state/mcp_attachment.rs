// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Test-support-only Managed-state construction now implements the required MCP effect port
// @formal reason: explicitly and fails closed for stage, publish and drain. Production composition still
// @formal reason: installs the canonical ManagedHost realizer; this adapter creates no MCP state, receipt
// @formal reason: or fallback path.
//! Default MCP attachment normalization policy.

/// Private fail-closed null adapter for applications that do not enable MCP
/// attachment commands. It is Session policy, not part of the public wire API.
pub(super) struct UnsupportedMcpAttachmentRealizer;

#[async_trait::async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for UnsupportedMcpAttachmentRealizer {
    async fn stage_mcp_attachment(
        &self,
        _request: crate::session_mcp::StageMcpAttachment,
    ) -> Result<crate::session_mcp::McpRealizationReceipt, awaken_session_types::RunError> {
        Err(awaken_session_types::RunError::classified(
            "mcp_runtime_unsupported",
            "fixture does not support generation-fenced MCP realization",
        ))
    }

    async fn publish_mcp_generation(
        &self,
        _generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), awaken_session_types::RunError> {
        Err(awaken_session_types::RunError::classified(
            "mcp_runtime_unsupported",
            "fixture does not support generation-fenced MCP publication",
        ))
    }

    async fn drain_mcp_generation(
        &self,
        _generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), awaken_session_types::RunError> {
        Err(awaken_session_types::RunError::classified(
            "mcp_runtime_unsupported",
            "fixture does not support generation-fenced MCP drain",
        ))
    }
}

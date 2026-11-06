// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: The canonical neutral MCP role preserves the original classified refusal for stage,
// @formal reason: publication and drain before any effect. Co-located production placements select their
// @formal reason: exact MCP owner; Worker-only placements carry no MCP effect capability.
//! The neutral MCP effect role of a non-owning Session fixture.

use super::NeutralRuntime;

#[async_trait::async_trait]
impl crate::McpAttachmentRealizer for NeutralRuntime {
    async fn stage_mcp_attachment(
        &self,
        _request: awaken_session_configuration_contract::mcp::StageMcpAttachment,
    ) -> Result<
        awaken_session_configuration_contract::mcp::McpRealizationReceipt,
        awaken_runtime_contract::runs::RunError,
    > {
        Err(awaken_runtime_contract::runs::RunError::classified(
            "mcp_runtime_unsupported",
            "fixture does not support generation-fenced MCP realization",
        ))
    }

    async fn publish_mcp_generation(
        &self,
        _generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Result<(), awaken_runtime_contract::runs::RunError> {
        Err(awaken_runtime_contract::runs::RunError::classified(
            "mcp_runtime_unsupported",
            "fixture does not support generation-fenced MCP publication",
        ))
    }

    async fn drain_mcp_generation(
        &self,
        _generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Result<(), awaken_runtime_contract::runs::RunError> {
        Err(awaken_runtime_contract::runs::RunError::classified(
            "mcp_runtime_unsupported",
            "fixture does not support generation-fenced MCP drain",
        ))
    }
}

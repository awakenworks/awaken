//! Default MCP attachment composition policy.

/// Private fail-closed null adapter for applications that do not enable MCP
/// attachment commands. It is composition policy, not part of the public port.
pub(super) struct UnsupportedMcpAttachmentRealizer;

#[async_trait::async_trait]
impl awaken_session_contract::McpAttachmentRealizer for UnsupportedMcpAttachmentRealizer {}

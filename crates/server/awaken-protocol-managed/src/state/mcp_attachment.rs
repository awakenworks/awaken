//! Default MCP attachment normalization policy.

/// Private fail-closed null adapter for applications that do not enable MCP
/// attachment commands. It is Session policy, not part of the public wire API.
pub(super) struct UnsupportedMcpAttachmentRealizer;

#[async_trait::async_trait]
impl awaken_session_contract::McpAttachmentRealizer for UnsupportedMcpAttachmentRealizer {}

//! Sampling handler for routing MCP `sampling/createMessage` requests to an LLM.

use async_trait::async_trait;
use mcp::transport::McpTransportError;
use mcp::{CreateMessageParams, CreateMessageResult};

/// Handler for MCP `sampling/createMessage` requests from the server.
///
/// When an MCP server sends a `sampling/createMessage` request during tool
/// execution, this handler is invoked to route it to an LLM for inference.
#[async_trait]
pub trait SamplingHandler: Send + Sync {
    async fn handle_create_message(
        &self,
        params: CreateMessageParams,
    ) -> Result<CreateMessageResult, McpTransportError>;
}

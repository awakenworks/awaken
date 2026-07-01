//! Stdio MCP transport.
//!
//! Built on the SDK's low-level [`AsyncStdioTransport`] (process spawn, JSON-RPC
//! framing, and the initialize handshake), but issuing `tools/list` and
//! `tools/call` directly so the raw [`CallToolResult`] — and thus its `isError`
//! flag — is preserved. The SDK's *high-level* `call_tool` collapses a tool
//! error into a transport error and discards `isError`, which would destroy the
//! three-state distinction [`McpRawTool`](crate::tool::McpRawTool) relies on.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use mcp::transport::McpTransportError;
use mcp::{
    AsyncStdioTransport, CallToolParams, CallToolResult, InitializeParams, ListToolsResult,
    McpToolDefinition,
};
use serde_json::Value;

use crate::transport::McpToolTransport;

/// Per-request timeout used when a caller does not supply one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// A stdio-spawned MCP server connection presented as an [`McpToolTransport`].
pub struct StdioTransport {
    inner: AsyncStdioTransport,
    timeout: Duration,
}

impl StdioTransport {
    /// Spawn `command args...` and complete the MCP handshake.
    pub async fn connect(
        command: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<Self, McpTransportError> {
        Self::connect_with_env(command, args, HashMap::new(), None, timeout).await
    }

    /// Spawn with extra environment variables and an optional `initialize`
    /// config payload.
    pub async fn connect_with_env(
        command: &str,
        args: &[String],
        env: HashMap<String, String>,
        config: Option<Value>,
        timeout: Duration,
    ) -> Result<Self, McpTransportError> {
        let inner = AsyncStdioTransport::spawn_with_env(command, args, env).await?;
        let transport = Self { inner, timeout };
        transport.initialize(config).await?;
        Ok(transport)
    }

    /// MCP lifecycle handshake: `initialize` then the `notifications/initialized`
    /// acknowledgement, matching the SDK adapter's own connect path. The
    /// acknowledgement is best-effort (some servers do not reply to it).
    async fn initialize(&self, config: Option<Value>) -> Result<(), McpTransportError> {
        let params = InitializeParams::new(config);
        self.inner
            .send_request_with_timeout(
                "initialize",
                Some(serde_json::to_value(&params)?),
                self.timeout,
            )
            .await?;
        let _ = self
            .inner
            .send_request_with_timeout(
                "notifications/initialized",
                Some(serde_json::json!({})),
                self.timeout,
            )
            .await;
        Ok(())
    }

    /// Whether the child process is still running.
    pub fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }

    /// Terminate the child process.
    pub async fn stop(&self) -> Result<(), McpTransportError> {
        self.inner.stop().await
    }
}

#[async_trait]
impl McpToolTransport for StdioTransport {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        let result = self
            .inner
            .send_request_with_timeout("tools/list", Some(serde_json::json!({})), self.timeout)
            .await?;
        let parsed: ListToolsResult = serde_json::from_value(result)?;
        Ok(parsed.tools)
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, McpTransportError> {
        let params = CallToolParams {
            name: tool_name.to_string(),
            arguments: Some(arguments),
            task: None,
            meta: None,
        };
        let result = self
            .inner
            .send_request_with_timeout(
                "tools/call",
                Some(serde_json::to_value(&params)?),
                self.timeout,
            )
            .await?;
        // Preserve the raw result — including `isError` — so the three-state
        // mapping in `McpRawTool` stays intact.
        let call_result: CallToolResult = serde_json::from_value(result)?;
        Ok(call_result)
    }
}

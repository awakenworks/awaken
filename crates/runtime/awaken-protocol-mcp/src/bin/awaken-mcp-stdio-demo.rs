//! Demo stdio MCP server over toy tools.
//!
//! Serves `echo` (plain) and `count` (progress-streaming) so any MCP client
//! can be pointed at a real subprocess — the crate's e2e test drives it with
//! the in-repo `awaken-ext-mcp` client, and it doubles as a manual smoke
//! target for external hosts:
//!
//! ```sh
//! cargo run -p awaken-protocol-mcp --bin awaken-mcp-stdio-demo
//! ```

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_mcp_wire::progress::McpProgressUpdate;
use awaken_protocol_mcp::export::ProgressRawTool;
use awaken_protocol_mcp::{McpExportedTool, McpStdioServer, McpToolService, StaticExports};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use serde_json::json;
use tokio::sync::mpsc;

struct EchoTool;

#[async_trait]
impl RawTool for EchoTool {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let message = call.arguments["message"].as_str().unwrap_or_default();
        Ok(ToolOutput::ok(call.call_id, format!("echo: {message}")))
    }
}

struct CountTool;

#[async_trait]
impl ProgressRawTool for CountTool {
    fn id(&self) -> &str {
        "count"
    }
    async fn invoke_with_progress(
        &self,
        call: ToolCall,
        progress: mpsc::Sender<McpProgressUpdate>,
    ) -> Result<ToolOutput, ToolError> {
        let steps = call.arguments["steps"].as_u64().unwrap_or(3).min(100);
        for step in 1..=steps {
            let _ = progress
                .send(McpProgressUpdate {
                    progress: step as f64,
                    total: Some(steps as f64),
                    message: Some(format!("step {step} of {steps}")),
                })
                .await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(ToolOutput::ok(call.call_id, format!("counted to {steps}")))
    }
}

fn exports() -> StaticExports {
    StaticExports::new(vec![
        McpExportedTool::plain(
            ToolDescriptor::pinned(
                "demo",
                "echo",
                "Echo the given message back.",
                json!({
                    "type": "object",
                    "properties": { "message": { "type": "string" } },
                    "required": ["message"],
                }),
            ),
            Arc::new(EchoTool),
        ),
        McpExportedTool::with_progress(
            ToolDescriptor::pinned(
                "demo",
                "count",
                "Count to `steps`, reporting progress for each step.",
                json!({
                    "type": "object",
                    "properties": { "steps": { "type": "integer", "minimum": 1 } },
                }),
            ),
            Arc::new(CountTool),
        ),
    ])
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let service = Arc::new(McpToolService::new(
        "awaken-mcp-stdio-demo",
        env!("CARGO_PKG_VERSION"),
        Arc::new(exports()),
    ));
    let server = McpStdioServer::serve_stdio(service);
    server.closed().await;
}

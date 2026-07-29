//! Neutral Host port for exporting a Session-owned tool to an ACP workload.
//!
//! The Host owns when a tool is projected and how long it lives. A higher
//! protocol adapter owns the concrete MCP server transport.

use std::sync::Arc;

pub struct AcpToolExport {
    pub server: awaken_run_executor_acp::McpServerConfig,
    _lease: Box<dyn Send + Sync>,
}

impl AcpToolExport {
    #[must_use]
    pub fn new(
        server: awaken_run_executor_acp::McpServerConfig,
        lease: impl Send + Sync + 'static,
    ) -> Self {
        Self {
            server,
            _lease: Box::new(lease),
        }
    }
}

#[async_trait::async_trait]
pub trait AcpToolExporter: Send + Sync {
    async fn export(
        &self,
        server_name: &str,
        descriptor: awaken_runtime_contract::resolved::ToolDescriptor,
        tool: Arc<dyn awaken_runtime_contract::tool::RawTool>,
    ) -> Result<AcpToolExport, String>;
}

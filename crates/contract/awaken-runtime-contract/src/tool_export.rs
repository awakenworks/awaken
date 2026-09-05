//! Session tool export port and its opaque lifetime lease.
//! Hosts own projection timing; MCP adapters implement transport assembly.

use std::sync::Arc;

pub struct AcpToolExport {
    pub server: crate::resolved::AcpMcpServer,
    _lease: Box<dyn Send + Sync>,
}

impl AcpToolExport {
    #[must_use]
    pub fn new(server: crate::resolved::AcpMcpServer, lease: impl Send + Sync + 'static) -> Self {
        Self {
            server,
            _lease: Box::new(lease),
        }
    }
}

#[async_trait::async_trait]
pub trait AcpToolExporter: Send + Sync {
    async fn export_set(
        &self,
        server_name: &str,
        descriptors: Vec<crate::resolved::ToolDescriptor>,
        tools: Vec<Arc<dyn crate::tool::RawTool>>,
    ) -> Result<AcpToolExport, String>;

    async fn export(
        &self,
        server_name: &str,
        descriptor: crate::resolved::ToolDescriptor,
        tool: Arc<dyn crate::tool::RawTool>,
    ) -> Result<AcpToolExport, String> {
        self.export_set(server_name, vec![descriptor], vec![tool])
            .await
    }
}

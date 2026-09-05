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
    /// Serve the same tool set on an already-bound process channel. The Host
    /// owns channel establishment and supplies the workload's exact stdio
    /// command; the protocol adapter owns framing. Unsupported adapters fail
    /// before publishing a transport they cannot serve.
    async fn export_channel_set(
        &self,
        _server: crate::resolved::AcpMcpServer,
        _descriptors: Vec<crate::resolved::ToolDescriptor>,
        _tools: Vec<Arc<dyn crate::tool::RawTool>>,
        _reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        _writer: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    ) -> Result<AcpToolExport, String> {
        Err("ACP tool exporter does not support attached channels".into())
    }

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

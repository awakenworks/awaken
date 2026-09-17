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
    /// The attached-channel capability of this exporter, if it has one.
    fn attached_channels(&self) -> Option<&dyn AttachedChannelToolExporter> {
        None
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

/// Optional capability of an [`AcpToolExporter`] that serves tools on an
/// attached process channel (ADR-0080 capability ladder).
#[async_trait::async_trait]
pub trait AttachedChannelToolExporter: AcpToolExporter {
    /// Serve the same tool set on the multiplexed attached process channel.
    /// Each local client owns an independent MCP peer; the export lease owns
    /// all peers. The Host supplies the workload's exact stdio command, the
    /// connection adapter owns byte substreams, and MCP owns their framing.
    async fn export_channel_set(
        &self,
        server: crate::resolved::AcpMcpServer,
        descriptors: Vec<crate::resolved::ToolDescriptor>,
        tools: Vec<Arc<dyn crate::tool::RawTool>>,
        reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        writer: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    ) -> Result<AcpToolExport, String>;
}

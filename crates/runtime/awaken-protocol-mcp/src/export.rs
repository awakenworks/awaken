//! The export surface: which runtime tools an MCP server serves.
//!
//! The unit is [`McpExportedTool`] — a pinned
//! [`ToolDescriptor`](awaken_runtime_contract::resolved::ToolDescriptor) paired
//! with its executable, which is either a plain
//! [`RawTool`](awaken_runtime_contract::tool::RawTool) or a
//! [`ProgressRawTool`] that streams progress while it runs. A
//! [`ToolExportSource`] supplies the set to [`McpToolService`](crate::service::McpToolService)
//! and versions it, so transports can emit `notifications/tools/list_changed`
//! when it changes.
//!
//! The export set is always explicit: hosts choose what crosses the boundary
//! (a [`DynamicTool`] converts directly for convenience), nothing is exported
//! by default.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use awaken_mcp_wire::progress::McpProgressUpdate;
use awaken_runtime_contract::plugin::DynamicTool;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use tokio::sync::{mpsc, watch};

/// A tool that reports progress while it runs. The server threads the per-call
/// channel through when the client sent a `progressToken`; each update becomes
/// one `notifications/progress`. Implemented alongside (not instead of) the
/// plain `RawTool` execution path — a tool with no progress to report stays a
/// plain `RawTool`.
#[async_trait]
pub trait ProgressRawTool: Send + Sync {
    fn id(&self) -> &str;
    async fn invoke_with_progress(
        &self,
        call: ToolCall,
        progress: mpsc::Sender<McpProgressUpdate>,
    ) -> Result<ToolOutput, ToolError>;
}

/// How an exported tool executes: the plain erased boundary, or the
/// progress-streaming variant.
#[derive(Clone)]
pub enum ToolExec {
    Plain(Arc<dyn RawTool>),
    WithProgress(Arc<dyn ProgressRawTool>),
}

/// One tool explicitly exported to external MCP clients: the model-visible
/// descriptor (`tools/list`) plus its executable (`tools/call`).
#[derive(Clone)]
pub struct McpExportedTool {
    pub descriptor: ToolDescriptor,
    pub exec: ToolExec,
}

impl McpExportedTool {
    pub fn plain(descriptor: ToolDescriptor, tool: Arc<dyn RawTool>) -> Self {
        Self {
            descriptor,
            exec: ToolExec::Plain(tool),
        }
    }

    pub fn with_progress(descriptor: ToolDescriptor, tool: Arc<dyn ProgressRawTool>) -> Self {
        Self {
            descriptor,
            exec: ToolExec::WithProgress(tool),
        }
    }
}

impl From<DynamicTool> for McpExportedTool {
    fn from(dynamic: DynamicTool) -> Self {
        Self::plain(dynamic.descriptor, dynamic.tool)
    }
}

/// The set of tools an MCP server serves. `version` is monotonic: a bump means
/// the set changed and transports owe connected clients a
/// `notifications/tools/list_changed`; `changes` hands them the signal to watch.
pub trait ToolExportSource: Send + Sync {
    /// Snapshot of the current export set.
    fn tools(&self) -> Vec<McpExportedTool>;

    /// Monotonic version of the set; fixed sources stay at 0.
    fn version(&self) -> u64 {
        0
    }

    /// Watch for version bumps. `None` for fixed sources — transports then
    /// skip the list_changed pump entirely.
    fn changes(&self) -> Option<watch::Receiver<u64>> {
        None
    }
}

/// A fixed export set — the common case: the host picks tools once at wiring
/// time.
pub struct StaticExports {
    tools: Vec<McpExportedTool>,
}

impl StaticExports {
    pub fn new(tools: Vec<McpExportedTool>) -> Self {
        Self { tools }
    }
}

impl ToolExportSource for StaticExports {
    fn tools(&self) -> Vec<McpExportedTool> {
        self.tools.clone()
    }
}

/// A replaceable export set: `replace` swaps the tools and bumps the version,
/// which drives `notifications/tools/list_changed` on connected transports.
pub struct SharedExports {
    tools: RwLock<Vec<McpExportedTool>>,
    version: watch::Sender<u64>,
}

impl SharedExports {
    pub fn new(tools: Vec<McpExportedTool>) -> Self {
        Self {
            tools: RwLock::new(tools),
            version: watch::channel(0).0,
        }
    }

    /// Swap the export set and bump the version.
    pub fn replace(&self, tools: Vec<McpExportedTool>) {
        *self.tools.write().expect("export set lock") = tools;
        self.version.send_modify(|v| *v += 1);
    }
}

impl ToolExportSource for SharedExports {
    fn tools(&self) -> Vec<McpExportedTool> {
        self.tools.read().expect("export set lock").clone()
    }

    fn version(&self) -> u64 {
        *self.version.borrow()
    }

    fn changes(&self) -> Option<watch::Receiver<u64>> {
        Some(self.version.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct NoopTool;

    #[async_trait]
    impl RawTool for NoopTool {
        fn id(&self) -> &str {
            "noop"
        }
        async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(call.call_id, "ok"))
        }
    }

    fn exported(id: &str) -> McpExportedTool {
        McpExportedTool::plain(
            ToolDescriptor::pinned("test", id, "a tool", json!({ "type": "object" })),
            Arc::new(NoopTool),
        )
    }

    #[test]
    fn static_exports_stay_at_version_zero_with_no_change_signal() {
        let source = StaticExports::new(vec![exported("a")]);
        assert_eq!(source.version(), 0);
        assert!(source.changes().is_none());
        assert_eq!(source.tools().len(), 1);
    }

    #[test]
    fn shared_exports_replace_bumps_the_version_and_signals() {
        let source = SharedExports::new(vec![exported("a")]);
        let changes = source.changes().expect("watchable");
        assert_eq!(source.version(), 0);

        source.replace(vec![exported("a"), exported("b")]);
        assert_eq!(source.version(), 1);
        assert!(changes.has_changed().expect("sender alive"));
        assert_eq!(source.tools().len(), 2);
    }

    #[test]
    fn dynamic_tool_converts_to_a_plain_export() {
        let dynamic = DynamicTool {
            descriptor: ToolDescriptor::pinned("test", "noop", "a tool", json!({})),
            tool: Arc::new(NoopTool),
        };
        let export: McpExportedTool = dynamic.into();
        assert_eq!(export.descriptor.id, "noop");
        assert!(matches!(export.exec, ToolExec::Plain(_)));
    }
}

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use awaken_mcp_server_core::NotifySink;
use awaken_mcp_server_testkit::{McpConformanceDriver, assert_mcp_server_conformance};
use awaken_mcp_wire::jsonrpc::ServerRequestError;
use awaken_mcp_wire::progress::McpProgressUpdate;
use awaken_protocol_mcp::{McpExportedTool, McpToolService, ProgressRawTool, StaticExports};
use awaken_runtime_contract::permission::ToolCall;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};
use serde_json::{Value, json};
use tokio::sync::mpsc;

struct Echo {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl RawTool for Echo {
    fn id(&self) -> &str {
        "echo"
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok(
            call.call_id,
            format!(
                "echo: {}",
                call.arguments["message"].as_str().unwrap_or_default()
            ),
        ))
    }
}

struct Progress {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ProgressRawTool for Progress {
    fn id(&self) -> &str {
        "progress"
    }

    async fn invoke_with_progress(
        &self,
        call: ToolCall,
        progress: mpsc::Sender<McpProgressUpdate>,
    ) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let steps = call.arguments["steps"].as_u64().unwrap_or(1);
        for step in 1..=steps {
            progress
                .send(McpProgressUpdate {
                    progress: step as f64,
                    total: Some(steps as f64),
                    message: None,
                })
                .await
                .expect("progress receiver");
        }
        Ok(ToolOutput::ok(call.call_id, format!("counted {steps}")))
    }
}

struct Driver {
    service: McpToolService,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl McpConformanceDriver for Driver {
    async fn request(
        &self,
        method: &str,
        params: Value,
        sink: &dyn NotifySink,
    ) -> Result<Value, ServerRequestError> {
        self.service.handle_with_sink(method, params, sink).await
    }

    fn host_call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn descriptor(id: &str) -> ToolDescriptor {
    ToolDescriptor::pinned("conformance", id, id, json!({ "type": "object" }))
}

#[tokio::test]
async fn awaken_adapter_passes_the_shared_neutral_suite() {
    let calls = Arc::new(AtomicUsize::new(0));
    let exports = StaticExports::new(vec![
        McpExportedTool::plain(
            descriptor("echo"),
            Arc::new(Echo {
                calls: calls.clone(),
            }),
        ),
        McpExportedTool::with_progress(
            descriptor("progress"),
            Arc::new(Progress {
                calls: calls.clone(),
            }),
        ),
    ]);
    let driver = Driver {
        service: McpToolService::new("awaken-adapter", "1", Arc::new(exports)),
        calls,
    };
    assert_mcp_server_conformance(&driver).await;
}

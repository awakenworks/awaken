use std::sync::Arc;

use awaken_mcp_server_core::NullSink;
use awaken_protocol_mcp::{McpExportedTool, McpToolService, StaticExports};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use serde_json::json;

struct ObservedTool(tokio::sync::mpsc::UnboundedSender<ToolCall>);

#[async_trait::async_trait]
impl RawTool for ObservedTool {
    fn id(&self) -> &str {
        "observed"
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.0.send(call.clone()).unwrap();
        if call.arguments["pending"] == true {
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::ok(call.call_id, "completed"))
    }
}

#[tokio::test]
async fn rebuilt_exports_cannot_reuse_an_in_flight_or_completed_call_identity() {
    // Cause/effect graph: one executor survives export reconstruction; each
    // independent MCP invocation needs a fresh fallback identity, otherwise the
    // Hand can mistake new work for its unresolved old operation. Decision rows:
    // I1 same export + identical arguments; I2 rebuilt export + identical args;
    // I3 cancellation after dispatch + rebuilt export. Every row preserves the
    // tool/arguments and has distinct ids. These are invocation ids, not durable
    // retry tokens: the existing relay tests keep the old operation indeterminate.
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let tool = Arc::new(ObservedTool(sent));
    let mut calls = Vec::new();
    for _ in 0..2 {
        let service = McpToolService::new(
            "awaken_session",
            "test",
            Arc::new(StaticExports::new(vec![McpExportedTool::plain(
                ToolDescriptor::pinned("test", "observed", "observed", json!({"type":"object"})),
                tool.clone(),
            )])),
        );
        for pending in [false, false, true] {
            let arguments = json!({"pending":pending});
            let invocation = service.handle(
                "tools/call",
                json!({"name":"observed", "arguments":arguments}),
                Arc::new(NullSink),
            );
            tokio::pin!(invocation);
            if pending {
                tokio::select! {
                    result = &mut invocation => panic!("pending call completed: {result:?}"),
                    call = received.recv() => calls.push(call.unwrap()),
                }
                // Dropping this future cancels after the RawTool observed dispatch.
            } else {
                assert_eq!(invocation.await.unwrap()["isError"], false);
                calls.push(received.recv().await.unwrap());
            }
            let call = calls.last().unwrap();
            assert_eq!(call.tool_id, "observed");
            assert_eq!(call.arguments, arguments);
        }
    }
    let unique = calls
        .iter()
        .map(|call| &call.call_id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        unique.len(),
        calls.len(),
        "I1-I3 independent invocations must not alias"
    );
}

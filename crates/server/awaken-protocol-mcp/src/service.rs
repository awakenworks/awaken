//! The transport-neutral MCP request handler.
//!
//! [`McpToolService`] maps the MCP server methods onto the runtime's tool
//! vocabulary:
//!
//! - `initialize`/`ping` — lifecycle;
//! - `tools/list` — the export set's [`ToolDescriptor`]s as MCP tool
//!   definitions;
//! - `tools/call` — gate, invoke, and map the neutral result back onto the
//!   MCP three-state (the mirror of `McpRawTool` in `awaken-ext-mcp`).
//!
//! A call carrying a `progressToken` in `_meta` has its
//! [`ProgressRawTool`](crate::export::ProgressRawTool) updates forwarded as
//! `notifications/progress` through the transport's [`NotifySink`] — all
//! updates are flushed before the final result is returned, so a client never
//! sees progress after the response.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use awaken_agent_contract::agent::state::Store;
use awaken_mcp_server_core::{McpCall, McpHostError, McpServer, McpToolHost};
use awaken_mcp_wire::jsonrpc::ServerRequestError;
use awaken_mcp_wire::progress::McpProgressUpdate;
use awaken_mcp_wire::{CallToolResult, McpToolDefinition, ToolContent};
use awaken_runtime_contract::permission::{GateOutcome, ToolGateHook};
use awaken_runtime_contract::tool::{ToolCall, ToolError, ToolOutput};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::export::{McpExportedTool, ToolExec, ToolExportSource};

pub use awaken_mcp_server_core::{NotifySink, NullSink};

/// The awaken-owned context presented to the neutral server port. External calls
/// currently start with empty state; keeping it explicit prevents the core from
/// assuming every future host has an awaken `Store`.
#[derive(Default)]
pub struct AwakenMcpContext {
    pub store: Store,
}

struct AwakenMcpHost {
    source: Arc<dyn ToolExportSource>,
    gate: RwLock<Option<Arc<dyn ToolGateHook>>>,
    next_call_id: AtomicU64,
}

/// Compatibility facade over the runtime-neutral server core.
pub struct McpToolService {
    core: McpServer<Arc<AwakenMcpHost>, AwakenMcpContext>,
    host: Arc<AwakenMcpHost>,
    context: AwakenMcpContext,
    source: Arc<dyn ToolExportSource>,
}

impl McpToolService {
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        source: Arc<dyn ToolExportSource>,
    ) -> Self {
        let tools_list_changed = source.changes().is_some();
        let host = Arc::new(AwakenMcpHost {
            source: Arc::clone(&source),
            gate: RwLock::new(None),
            next_call_id: AtomicU64::new(1),
        });
        Self {
            core: McpServer::new(name, version, Arc::clone(&host))
                .with_tools_list_changed(tools_list_changed),
            host,
            context: AwakenMcpContext::default(),
            source,
        }
    }

    /// Gate every external `tools/call` (deny/ask outcomes become model-visible
    /// error results, never silent execution).
    #[must_use]
    pub fn with_gate(self, gate: Arc<dyn ToolGateHook>) -> Self {
        *self.host.gate.write().expect("MCP gate lock") = Some(gate);
        self
    }

    /// The export set this service serves — transports watch it for
    /// `tools/list_changed`.
    pub fn source(&self) -> &Arc<dyn ToolExportSource> {
        &self.source
    }

    /// Dispatch one client request. `sink` carries `notifications/progress`
    /// emitted while the request runs.
    pub async fn handle(
        &self,
        method: &str,
        params: Value,
        sink: Arc<dyn NotifySink>,
    ) -> Result<Value, ServerRequestError> {
        self.handle_with_sink(method, params, sink.as_ref()).await
    }

    /// Borrowed-sink variant used by the neutral conformance driver and thin
    /// transports; the Arc-taking method above remains source-compatible.
    pub async fn handle_with_sink(
        &self,
        method: &str,
        params: Value,
        sink: &dyn NotifySink,
    ) -> Result<Value, ServerRequestError> {
        self.core.handle(&self.context, method, params, sink).await
    }

    /// Dispatch with the JSON-RPC id available to the core cancellation registry.
    pub(crate) async fn handle_request(
        &self,
        id: &Value,
        method: &str,
        params: Value,
        sink: &dyn NotifySink,
    ) -> Result<Value, ServerRequestError> {
        self.core
            .handle_request(&self.context, Some(id), method, params, sink)
            .await
    }

    pub(crate) async fn handle_http(
        &self,
        headers: &axum::http::HeaderMap,
        body: &[u8],
        sink: &dyn NotifySink,
    ) -> awaken_mcp_server_core::McpHttpReply {
        awaken_mcp_server_core::handle_streamable_http(
            &self.core,
            &self.context,
            awaken_mcp_server_core::McpHttpMethod::Post,
            headers,
            body,
            &awaken_mcp_server_core::AllowAllOrigins,
            sink,
        )
        .await
    }

    pub async fn handle_notification(&self, method: &str, params: &Value) {
        self.core
            .handle_notification(&self.context, method, params)
            .await;
    }
}

#[async_trait]
impl McpToolHost<AwakenMcpContext> for AwakenMcpHost {
    async fn list_tools(
        &self,
        _context: &AwakenMcpContext,
    ) -> Result<Vec<McpToolDefinition>, McpHostError> {
        Ok(self
            .source
            .tools()
            .into_iter()
            .map(|tool| {
                McpToolDefinition::new(tool.descriptor.id)
                    .with_description(tool.descriptor.description)
                    .with_schema(tool.descriptor.parameters)
            })
            .collect())
    }

    async fn call_tool(
        &self,
        context: &AwakenMcpContext,
        call: McpCall,
        notifications: &dyn NotifySink,
    ) -> Result<CallToolResult, McpHostError> {
        let exported = self
            .source
            .tools()
            .into_iter()
            .find(|tool| tool.descriptor.id == call.name)
            .ok_or_else(|| McpHostError::NotFound(format!("unknown tool: {}", call.name)))?;
        let progress_token = call
            .meta
            .as_ref()
            .and_then(|meta| meta.get("progressToken"))
            .filter(|token| !token.is_null())
            .cloned();
        let call = ToolCall {
            call_id: format!(
                "mcp-srv-{}",
                self.next_call_id.fetch_add(1, Ordering::SeqCst)
            ),
            tool_id: call.name,
            arguments: call.arguments,
        };

        if let Some(result) = self.gate_verdict(context, &call).await {
            return Ok(result);
        }
        match invoke(&exported, call, progress_token, notifications).await {
            Ok(output) => Ok(call_result(&output)),
            Err(ToolError::Unknown(id)) => {
                Err(McpHostError::NotFound(format!("unknown tool: {id}")))
            }
            Err(ToolError::InvalidArguments(message)) => {
                Err(McpHostError::InvalidArguments(message))
            }
            Err(ToolError::Execution(message)) => Err(McpHostError::Internal(message)),
        }
    }
}

impl AwakenMcpHost {
    /// Consult the awaken runtime gate. Outcomes requiring a suspended run fail
    /// closed because an external MCP call owns no run lifecycle.
    async fn gate_verdict(
        &self,
        context: &AwakenMcpContext,
        call: &ToolCall,
    ) -> Option<CallToolResult> {
        let gate = self.gate.read().expect("MCP gate lock").clone()?;
        match gate.gate(call, &context.store).await {
            GateOutcome::Allow => None,
            GateOutcome::Block { reason } => Some(call_result(&ToolOutput::error(
                call.call_id.clone(),
                format!("blocked: {reason}"),
            ))),
            GateOutcome::SetResult(output) => Some(call_result(&output)),
            GateOutcome::RequireConfirmation { .. } | GateOutcome::Schedule { .. } => {
                Some(call_result(&ToolOutput::error(
                    call.call_id.clone(),
                    "tool call requires out-of-band approval, which is not available to an external MCP client",
                )))
            }
        }
    }
}

/// Run the exported tool. A progress-capable tool with a client token streams
/// each update as `notifications/progress` (fully flushed before returning);
/// without a token its updates are drained and dropped.
async fn invoke(
    exported: &McpExportedTool,
    call: ToolCall,
    progress_token: Option<Value>,
    sink: &dyn NotifySink,
) -> Result<ToolOutput, ToolError> {
    let progress_tool = match &exported.exec {
        ToolExec::Plain(tool) => return tool.invoke(call).await,
        ToolExec::WithProgress(tool) => tool,
    };
    let (tx, mut rx) = mpsc::channel::<McpProgressUpdate>(64);
    let forwarder = async {
        while let Some(update) = rx.recv().await {
            let Some(token) = &progress_token else {
                continue;
            };
            let mut params = json!({
                "progressToken": token,
                "progress": update.progress,
            });
            if let Some(total) = update.total {
                params["total"] = json!(total);
            }
            if let Some(message) = &update.message {
                params["message"] = json!(message);
            }
            sink.notify("notifications/progress", params).await;
        }
    };
    let invocation = progress_tool.invoke_with_progress(call, tx);
    let (result, ()) = tokio::join!(invocation, forwarder);
    // The tool dropped its sender (or never took one past its return): the
    // forwarder drains what was queued and ends — awaiting it orders every
    // progress notification before the final response.
    result
}

/// Project the neutral output onto the wire result: the content string as one
/// text block, `is_error` preserved. `state` is run-internal and dropped here.
fn call_result(output: &ToolOutput) -> CallToolResult {
    CallToolResult {
        content: vec![ToolContent::Text {
            text: output.content.clone(),
            annotations: None,
            meta: None,
        }],
        structured_content: None,
        is_error: Some(output.is_error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{ProgressRawTool, SharedExports, StaticExports};
    use awaken_runtime_contract::resolved::ToolDescriptor;
    use awaken_runtime_contract::tool::RawTool;
    use std::sync::Mutex;

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

    struct FailingTool;

    #[async_trait]
    impl RawTool for FailingTool {
        fn id(&self) -> &str {
            "failing"
        }
        async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::error(call.call_id, "boom"))
        }
    }

    struct BrokenTool;

    #[async_trait]
    impl RawTool for BrokenTool {
        fn id(&self) -> &str {
            "broken"
        }
        async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
            Err(ToolError::Execution("wire snapped".to_string()))
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
            let steps = call.arguments["steps"].as_u64().unwrap_or(2);
            for step in 1..=steps {
                let _ = progress
                    .send(McpProgressUpdate {
                        progress: step as f64,
                        total: Some(steps as f64),
                        message: Some(format!("step {step}")),
                    })
                    .await;
            }
            Ok(ToolOutput::ok(call.call_id, format!("counted {steps}")))
        }
    }

    /// Collects notifications for assertions.
    #[derive(Default)]
    struct RecordingSink {
        seen: Mutex<Vec<(String, Value)>>,
    }

    #[async_trait]
    impl NotifySink for RecordingSink {
        async fn notify(&self, method: &str, params: Value) {
            self.seen.lock().unwrap().push((method.to_string(), params));
        }
    }

    fn descriptor(id: &str) -> ToolDescriptor {
        ToolDescriptor::pinned(
            "test",
            id,
            format!("the {id} tool"),
            json!({
                "type": "object"
            }),
        )
    }

    fn service() -> McpToolService {
        let source = StaticExports::new(vec![
            McpExportedTool::plain(descriptor("echo"), Arc::new(EchoTool)),
            McpExportedTool::plain(descriptor("failing"), Arc::new(FailingTool)),
            McpExportedTool::plain(descriptor("broken"), Arc::new(BrokenTool)),
            McpExportedTool::with_progress(descriptor("count"), Arc::new(CountTool)),
        ]);
        McpToolService::new("test-server", "0.0.0", Arc::new(source))
    }

    async fn handle(
        service: &McpToolService,
        method: &str,
        params: Value,
    ) -> Result<Value, ServerRequestError> {
        service.handle(method, params, Arc::new(NullSink)).await
    }

    #[tokio::test]
    async fn initialize_does_not_advertise_list_changed_for_static_tools() {
        let result = handle(
            &service(),
            "initialize",
            json!({ "protocolVersion": "2025-06-18" }),
        )
        .await
        .expect("initializes");
        assert_eq!(result["protocolVersion"], "2025-06-18");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(result["serverInfo"]["name"], "test-server");
    }

    #[tokio::test]
    async fn initialize_advertises_list_changed_for_a_watchable_tool_source() {
        let source = SharedExports::new(vec![McpExportedTool::plain(
            descriptor("echo"),
            Arc::new(EchoTool),
        )]);
        let service = McpToolService::new("dynamic", "1", Arc::new(source));
        let result = handle(
            &service,
            "initialize",
            json!({ "protocolVersion": "2025-06-18" }),
        )
        .await
        .expect("dynamic server initializes");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], true);
    }

    #[tokio::test]
    async fn tools_list_projects_the_descriptors() {
        let result = handle(&service(), "tools/list", json!({}))
            .await
            .expect("lists");
        let tools = result["tools"].as_array().expect("array");
        assert_eq!(tools.len(), 4);
        let echo = tools.iter().find(|t| t["name"] == "echo").expect("echo");
        assert_eq!(echo["description"], "the echo tool");
        assert_eq!(echo["inputSchema"]["type"], "object");
    }

    #[tokio::test]
    async fn call_success_maps_to_a_text_result() {
        let result = handle(
            &service(),
            "tools/call",
            json!({ "name": "echo", "arguments": { "message": "hi" } }),
        )
        .await
        .expect("calls");
        assert_eq!(result["content"][0]["text"], "echo: hi");
        assert_eq!(result["isError"], false);
    }

    #[tokio::test]
    async fn tool_level_failure_stays_a_result_with_is_error() {
        let result = handle(&service(), "tools/call", json!({ "name": "failing" }))
            .await
            .expect("returns a result, not a protocol error");
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "boom");
    }

    #[tokio::test]
    async fn execution_error_becomes_a_jsonrpc_error() {
        let err = handle(&service(), "tools/call", json!({ "name": "broken" }))
            .await
            .expect_err("protocol-level failure");
        assert_eq!(err.code, -32603);
        assert!(err.message.contains("wire snapped"));
    }

    #[tokio::test]
    async fn unknown_tool_is_invalid_params() {
        let err = handle(&service(), "tools/call", json!({ "name": "nope" }))
            .await
            .expect_err("unknown tool");
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("nope"));
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let err = handle(&service(), "resources/list", json!({}))
            .await
            .expect_err("not served");
        assert_eq!(err.code, -32601);
    }

    #[tokio::test]
    async fn progress_token_streams_updates_before_the_result() {
        let sink = Arc::new(RecordingSink::default());
        let service = service();
        let result = service
            .handle(
                "tools/call",
                json!({
                    "name": "count",
                    "arguments": { "steps": 3 },
                    "_meta": { "progressToken": 7 },
                }),
                Arc::clone(&sink) as Arc<dyn NotifySink>,
            )
            .await
            .expect("calls");
        assert_eq!(result["content"][0]["text"], "counted 3");

        let seen = sink.seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "one notification per step");
        for (index, (method, params)) in seen.iter().enumerate() {
            assert_eq!(method, "notifications/progress");
            assert_eq!(params["progressToken"], 7);
            assert_eq!(params["progress"], (index + 1) as f64);
            assert_eq!(params["total"], 3.0);
        }
    }

    #[tokio::test]
    async fn progress_without_a_token_is_dropped() {
        let sink = Arc::new(RecordingSink::default());
        let service = service();
        let result = service
            .handle(
                "tools/call",
                json!({ "name": "count", "arguments": { "steps": 2 } }),
                Arc::clone(&sink) as Arc<dyn NotifySink>,
            )
            .await
            .expect("calls");
        assert_eq!(result["content"][0]["text"], "counted 2");
        assert!(
            sink.seen.lock().unwrap().is_empty(),
            "no token, no notifications"
        );
    }

    #[tokio::test]
    async fn a_progress_token_on_a_non_progress_tool_is_ignored() {
        // `echo` is a plain tool; a `progressToken` cannot make it stream — the
        // call returns the plain result and emits no notifications.
        let sink = Arc::new(RecordingSink::default());
        let service = service();
        let result = service
            .handle(
                "tools/call",
                json!({
                    "name": "echo",
                    "arguments": { "message": "hi" },
                    "_meta": { "progressToken": 9 },
                }),
                Arc::clone(&sink) as Arc<dyn NotifySink>,
            )
            .await
            .expect("calls");
        assert_eq!(result["content"][0]["text"], "echo: hi");
        assert!(
            sink.seen.lock().unwrap().is_empty(),
            "a plain tool emits no progress even when a token is supplied"
        );
    }

    struct DenyGate;

    #[async_trait]
    impl ToolGateHook for DenyGate {
        async fn gate(&self, ctx: &ToolCall, _state: &Store) -> GateOutcome {
            GateOutcome::Block {
                reason: format!("{} is not allowed here", ctx.tool_id),
            }
        }
    }

    #[tokio::test]
    async fn gate_block_is_a_model_visible_error_result() {
        let source = StaticExports::new(vec![McpExportedTool::plain(
            descriptor("echo"),
            Arc::new(EchoTool),
        )]);
        let service = McpToolService::new("test-server", "0.0.0", Arc::new(source))
            .with_gate(Arc::new(DenyGate));
        let result = handle(&service, "tools/call", json!({ "name": "echo" }))
            .await
            .expect("blocked is a result, not a protocol error");
        assert_eq!(result["isError"], true);
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("echo is not allowed here")
        );
    }

    struct SetResultGate;

    #[async_trait]
    impl ToolGateHook for SetResultGate {
        async fn gate(&self, ctx: &ToolCall, _state: &Store) -> GateOutcome {
            GateOutcome::SetResult(ToolOutput::ok(ctx.call_id.clone(), "supplied by the gate"))
        }
    }

    #[tokio::test]
    async fn gate_set_result_supplies_the_output_without_running_the_tool() {
        let source = StaticExports::new(vec![McpExportedTool::plain(
            descriptor("echo"),
            Arc::new(EchoTool),
        )]);
        let service = McpToolService::new("test-server", "0.0.0", Arc::new(source))
            .with_gate(Arc::new(SetResultGate));
        let result = handle(
            &service,
            "tools/call",
            json!({ "name": "echo", "arguments": { "message": "hi" } }),
        )
        .await
        .expect("the gate supplies a result");
        assert_eq!(result["isError"], false);
        // The gate's output is returned; the tool never ran (no "echo: hi").
        assert_eq!(result["content"][0]["text"], "supplied by the gate");
    }

    struct ScheduleGate;

    #[async_trait]
    impl ToolGateHook for ScheduleGate {
        async fn gate(&self, _ctx: &ToolCall, _state: &Store) -> GateOutcome {
            GateOutcome::Schedule {
                correlation_id: "corr-1".to_string(),
                action_kind: None,
            }
        }
    }

    #[tokio::test]
    async fn gate_schedule_fails_closed_like_suspend() {
        // Schedule awaits a *run*; an external MCP client has none, so — like
        // Suspend — it fails closed as a model-visible refusal (distinct enum arm).
        let source = StaticExports::new(vec![McpExportedTool::plain(
            descriptor("echo"),
            Arc::new(EchoTool),
        )]);
        let service = McpToolService::new("test-server", "0.0.0", Arc::new(source))
            .with_gate(Arc::new(ScheduleGate));
        let result = handle(&service, "tools/call", json!({ "name": "echo" }))
            .await
            .expect("schedule is a model-visible refusal, not a protocol error");
        assert_eq!(result["isError"], true);
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("out-of-band approval")
        );
    }

    struct AllowGate;

    #[async_trait]
    impl ToolGateHook for AllowGate {
        async fn gate(&self, _ctx: &ToolCall, _state: &Store) -> GateOutcome {
            GateOutcome::Allow
        }
    }

    #[tokio::test]
    async fn gate_allow_lets_the_tool_execute() {
        // A gate is wired and allows — the `None ⇒ execute` path: the tool runs.
        let source = StaticExports::new(vec![McpExportedTool::plain(
            descriptor("echo"),
            Arc::new(EchoTool),
        )]);
        let service = McpToolService::new("test-server", "0.0.0", Arc::new(source))
            .with_gate(Arc::new(AllowGate));
        let result = handle(
            &service,
            "tools/call",
            json!({ "name": "echo", "arguments": { "message": "hi" } }),
        )
        .await
        .expect("allow runs the tool");
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "echo: hi");
    }

    #[tokio::test]
    async fn ping_returns_an_empty_result() {
        let result = handle(&service(), "ping", json!({})).await.expect("ping");
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn initialize_reports_the_server_name_and_version() {
        let result = handle(
            &service(),
            "initialize",
            json!({ "protocolVersion": "2025-06-18" }),
        )
        .await
        .expect("initializes");
        assert_eq!(result["serverInfo"]["name"], "test-server");
        assert_eq!(result["serverInfo"]["version"], "0.0.0");
    }

    #[tokio::test]
    async fn initialize_without_a_version_is_invalid_params() {
        let error = handle(&service(), "initialize", json!({}))
            .await
            .expect_err("a version is required for exact negotiation");
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("protocolVersion"));
    }

    #[tokio::test]
    async fn malformed_tools_call_params_is_invalid_params() {
        // No `name` — the CallToolParams decode fails before dispatch.
        let err = handle(
            &service(),
            "tools/call",
            json!({ "arguments": { "message": "hi" } }),
        )
        .await
        .expect_err("missing name is a decode failure");
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("tools/call"));
    }

    struct BadArgsTool;

    #[async_trait]
    impl RawTool for BadArgsTool {
        fn id(&self) -> &str {
            "badargs"
        }
        async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
            Err(ToolError::InvalidArguments("path is required".to_string()))
        }
    }

    #[tokio::test]
    async fn invoke_time_invalid_arguments_is_invalid_params() {
        let source = StaticExports::new(vec![McpExportedTool::plain(
            descriptor("badargs"),
            Arc::new(BadArgsTool),
        )]);
        let service = McpToolService::new("test-server", "0.0.0", Arc::new(source));
        let err = handle(&service, "tools/call", json!({ "name": "badargs" }))
            .await
            .expect_err("invalid arguments at invoke time");
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("path is required"));
    }

    struct UnknownAtInvokeTool;

    #[async_trait]
    impl RawTool for UnknownAtInvokeTool {
        fn id(&self) -> &str {
            "unk"
        }
        async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
            Err(ToolError::Unknown("unk".to_string()))
        }
    }

    #[tokio::test]
    async fn invoke_time_unknown_is_invalid_params() {
        let source = StaticExports::new(vec![McpExportedTool::plain(
            descriptor("unk"),
            Arc::new(UnknownAtInvokeTool),
        )]);
        let service = McpToolService::new("test-server", "0.0.0", Arc::new(source));
        let err = handle(&service, "tools/call", json!({ "name": "unk" }))
            .await
            .expect_err("unknown tool at invoke time");
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("unknown tool: unk"));
    }

    /// A progress tool whose updates carry neither a `total` nor a `message`:
    /// exercises the "field absent" arm of the notification builder.
    struct BareProgressTool;

    #[async_trait]
    impl ProgressRawTool for BareProgressTool {
        fn id(&self) -> &str {
            "bare"
        }
        async fn invoke_with_progress(
            &self,
            call: ToolCall,
            progress: mpsc::Sender<McpProgressUpdate>,
        ) -> Result<ToolOutput, ToolError> {
            let _ = progress
                .send(McpProgressUpdate {
                    progress: 1.0,
                    total: None,
                    message: None,
                })
                .await;
            Ok(ToolOutput::ok(call.call_id, "done"))
        }
    }

    #[tokio::test]
    async fn a_progress_update_without_total_or_message_omits_those_keys() {
        // The forwarder only writes `total`/`message` when the update carries
        // them — a bare update yields a notification with just progressToken and
        // progress, so a client never sees a null total or message.
        let source = StaticExports::new(vec![McpExportedTool::with_progress(
            descriptor("bare"),
            Arc::new(BareProgressTool),
        )]);
        let service = McpToolService::new("test-server", "0.0.0", Arc::new(source));
        let sink = Arc::new(RecordingSink::default());
        let result = service
            .handle(
                "tools/call",
                json!({ "name": "bare", "_meta": { "progressToken": 1 } }),
                Arc::clone(&sink) as Arc<dyn NotifySink>,
            )
            .await
            .expect("calls");
        assert_eq!(result["content"][0]["text"], "done");

        let seen = sink.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "one bare update");
        let (method, params) = &seen[0];
        assert_eq!(method, "notifications/progress");
        assert_eq!(params["progressToken"], 1);
        assert_eq!(params["progress"], 1.0);
        assert!(
            params.get("total").is_none(),
            "no total key when the update has none"
        );
        assert!(
            params.get("message").is_none(),
            "no message key when the update has none"
        );
    }

    #[tokio::test]
    async fn a_null_progress_token_is_treated_as_absent() {
        // `_meta.progressToken: null` is not a subscription — the null-filter
        // drops it, so a progress-capable tool streams nothing (the mirror of
        // the HTTP transport's `is_null` guard).
        let sink = Arc::new(RecordingSink::default());
        let service = service();
        let result = service
            .handle(
                "tools/call",
                json!({
                    "name": "count",
                    "arguments": { "steps": 2 },
                    "_meta": { "progressToken": null },
                }),
                Arc::clone(&sink) as Arc<dyn NotifySink>,
            )
            .await
            .expect("calls");
        assert_eq!(result["content"][0]["text"], "counted 2");
        assert!(
            sink.seen.lock().unwrap().is_empty(),
            "a null token carries no client subscription"
        );
    }

    #[tokio::test]
    async fn initialize_with_a_non_string_version_is_invalid_params() {
        let error = handle(
            &service(),
            "initialize",
            json!({ "protocolVersion": 20_250_618 }),
        )
        .await
        .expect_err("a non-string version cannot be negotiated");
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("protocolVersion"));
    }
}

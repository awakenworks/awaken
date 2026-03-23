//! JSON-RPC 2.0 stdio server for ACP protocol.
//!
//! Reads line-delimited JSON-RPC 2.0 requests from stdin, dispatches them,
//! and writes responses/notifications to stdout.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use awaken_contract::contract::message::Message;
use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};
use awaken_runtime::AgentRuntime;

use super::encoder::AcpEncoder;

/// JSON-RPC 2.0 request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub id: Option<Value>,
}

/// JSON-RPC 2.0 response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    pub id: Option<Value>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC 2.0 notification envelope (no id).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcResponse {
    /// Create a success response.
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: Some(result),
            error: None,
            id,
        }
    }

    /// Create an error response.
    pub fn error(id: Option<Value>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
            id,
        }
    }

    /// Method not found error (-32601).
    pub fn method_not_found(id: Option<Value>) -> Self {
        Self::error(id, -32601, "Method not found")
    }

    /// Invalid params error (-32602).
    pub fn invalid_params(id: Option<Value>, message: impl Into<String>) -> Self {
        Self::error(id, -32602, message)
    }

    /// Internal error (-32603).
    pub fn internal_error(id: Option<Value>, message: impl Into<String>) -> Self {
        Self::error(id, -32603, message)
    }
}

impl JsonRpcNotification {
    /// Create a notification.
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.into(),
            params: Some(params),
        }
    }
}

/// Parse a JSON-RPC 2.0 request from a line.
pub fn parse_request(line: &str) -> Result<JsonRpcRequest, String> {
    serde_json::from_str(line).map_err(|e| format!("invalid JSON-RPC request: {e}"))
}

/// Serialize a JSON-RPC 2.0 response to a line.
pub fn serialize_response(response: &JsonRpcResponse) -> String {
    serde_json::to_string(response).unwrap_or_else(|_| {
        r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"serialization error"},"id":null}"#
            .to_string()
    })
}

/// Serialize a JSON-RPC 2.0 notification to a line.
pub fn serialize_notification(notification: &JsonRpcNotification) -> String {
    serde_json::to_string(notification)
        .unwrap_or_else(|_| r#"{"jsonrpc":"2.0","method":"error","params":null}"#.to_string())
}

// ── Stdio server ────────────────────────────────────────────────────

/// Server capabilities returned by `initialize`.
fn server_capabilities() -> Value {
    serde_json::json!({
        "protocolVersion": "0.1.0",
        "serverInfo": {
            "name": "awaken-acp-stdio",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": {
            "streaming": true,
            "toolCallNotifications": true,
            "permissionFlow": true,
        },
    })
}

/// Write a single line to the given writer and flush.
async fn write_line<W: AsyncWriteExt + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

/// Run the ACP stdio server, reading from `input` and writing to `output`.
///
/// This generic form accepts any `AsyncBufRead` + `AsyncWrite` to enable
/// testing without actual stdin/stdout.
pub async fn serve_stdio_io<R, W>(runtime: Arc<AgentRuntime>, input: R, mut output: W)
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut lines = input.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let request = match parse_request(&line) {
            Ok(req) => req,
            Err(e) => {
                let resp = JsonRpcResponse::error(None, -32700, format!("Parse error: {e}"));
                let _ = write_line(&mut output, &serialize_response(&resp)).await;
                continue;
            }
        };

        match request.method.as_str() {
            "initialize" => {
                let resp = JsonRpcResponse::success(request.id, server_capabilities());
                let _ = write_line(&mut output, &serialize_response(&resp)).await;
            }
            "run_prompt" => {
                handle_run_prompt(runtime.clone(), request, &mut output).await;
            }
            "session/update" => {
                // Tool call decision notification (from client).
                handle_session_update(&runtime, request, &mut output).await;
            }
            "session/request_permission" => {
                // Permission decision from client.
                handle_permission_decision(&runtime, request, &mut output).await;
            }
            _ => {
                let resp = JsonRpcResponse::method_not_found(request.id);
                let _ = write_line(&mut output, &serialize_response(&resp)).await;
            }
        }
    }
}

/// Run the ACP stdio server on actual stdin/stdout.
pub async fn serve_stdio(runtime: Arc<AgentRuntime>) {
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_stdio_io(runtime, stdin, stdout).await;
}

/// Handle `run_prompt` — execute an agent run and stream events as notifications.
async fn handle_run_prompt<W: AsyncWriteExt + Unpin>(
    runtime: Arc<AgentRuntime>,
    request: JsonRpcRequest,
    output: &mut W,
) {
    let params = request.params.unwrap_or(Value::Null);

    let agent_id = params
        .get("agentId")
        .or_else(|| params.get("agent_id"))
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_string();

    let thread_id = params
        .get("threadId")
        .or_else(|| params.get("thread_id"))
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    let text = params
        .get("message")
        .or_else(|| params.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("");

    if text.is_empty() {
        let resp = JsonRpcResponse::invalid_params(request.id, "message text is required");
        let _ = write_line(output, &serialize_response(&resp)).await;
        return;
    }

    let messages = vec![Message::user(text)];

    // ACK the request immediately with the run/thread identifiers
    let run_id = uuid::Uuid::now_v7().to_string();
    let resp = JsonRpcResponse::success(
        request.id,
        serde_json::json!({
            "runId": run_id,
            "threadId": thread_id,
        }),
    );
    let _ = write_line(output, &serialize_response(&resp)).await;

    // Execute the run and stream events via a channel
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = crate::transport::channel_sink::ChannelEventSink::new(event_tx);
    let run_request = awaken_runtime::RunRequest::new(thread_id, messages).with_agent_id(agent_id);

    // Spawn the run in a background task so we can stream events synchronously
    let rt = runtime.clone();
    let run_handle = tokio::spawn(async move {
        if let Err(e) = rt.run(run_request, Arc::new(sink)).await {
            tracing::warn!(error = %e, "stdio run failed");
        }
        // sink is dropped here, closing event_rx
    });

    // Stream events as JSON-RPC notifications
    let mut encoder = AcpEncoder::new();
    while let Some(event) = event_rx.recv().await {
        let acp_events = encoder.on_agent_event(&event);
        for acp_ev in acp_events {
            if let Ok(params) = serde_json::to_value(&acp_ev) {
                let method = match &acp_ev {
                    super::encoder::AcpEvent::SessionUpdate(_) => "session/update",
                    super::encoder::AcpEvent::RequestPermission(_) => "session/request_permission",
                };
                let notif = JsonRpcNotification::new(method, params);
                let _ = write_line(output, &serialize_notification(&notif)).await;
            }
        }
    }

    // Ensure the run task completes
    let _ = run_handle.await;
}

/// Handle `session/update` — forward tool call decisions.
async fn handle_session_update<W: AsyncWriteExt + Unpin>(
    runtime: &AgentRuntime,
    request: JsonRpcRequest,
    output: &mut W,
) {
    let params = request.params.unwrap_or(Value::Null);

    let thread_id = params
        .get("threadId")
        .or_else(|| params.get("thread_id"))
        .and_then(Value::as_str)
        .unwrap_or("");

    let tool_call_id = params
        .get("toolCallId")
        .or_else(|| params.get("tool_call_id"))
        .and_then(Value::as_str)
        .unwrap_or("");

    let action_str = params
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("resume");

    if thread_id.is_empty() || tool_call_id.is_empty() {
        let resp =
            JsonRpcResponse::invalid_params(request.id, "threadId and toolCallId are required");
        let _ = write_line(output, &serialize_response(&resp)).await;
        return;
    }

    let action = match action_str {
        "resume" => ResumeDecisionAction::Resume,
        "cancel" => ResumeDecisionAction::Cancel,
        _ => ResumeDecisionAction::Resume,
    };

    let resume = ToolCallResume {
        decision_id: uuid::Uuid::now_v7().to_string(),
        action,
        result: params.get("result").cloned().unwrap_or(Value::Null),
        reason: params
            .get("reason")
            .and_then(Value::as_str)
            .map(String::from),
        updated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    };

    let sent = runtime.send_decisions(thread_id, vec![(tool_call_id.to_string(), resume)]);

    if sent {
        // Notification — no response needed unless there's an id
        if let Some(id) = request.id {
            let resp = JsonRpcResponse::success(Some(id), serde_json::json!({"ok": true}));
            let _ = write_line(output, &serialize_response(&resp)).await;
        }
    } else if let Some(id) = request.id {
        let resp = JsonRpcResponse::internal_error(Some(id), "no active run for thread");
        let _ = write_line(output, &serialize_response(&resp)).await;
    }
}

/// Handle `session/request_permission` — forward permission decisions.
async fn handle_permission_decision<W: AsyncWriteExt + Unpin>(
    runtime: &AgentRuntime,
    request: JsonRpcRequest,
    output: &mut W,
) {
    // Re-use session/update logic — permission decisions are a specific kind of tool call resume
    handle_session_update(runtime, request, output).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Stub resolver that always returns an error (no agents registered).
    /// Used for testing the stdio transport layer without a real agent.
    struct StubResolver;
    impl awaken_runtime::AgentResolver for StubResolver {
        fn resolve(
            &self,
            agent_id: &str,
        ) -> Result<awaken_runtime::ResolvedAgent, awaken_runtime::RuntimeError> {
            Err(awaken_runtime::RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }
    }

    fn test_runtime() -> Arc<AgentRuntime> {
        Arc::new(AgentRuntime::new(Arc::new(StubResolver)))
    }

    #[test]
    fn parse_valid_request() {
        let line = r#"{"jsonrpc":"2.0","method":"session/start","params":{"agentId":"a1"},"id":1}"#;
        let req = parse_request(line).unwrap();
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.method, "session/start");
        assert_eq!(req.id, Some(json!(1)));
    }

    #[test]
    fn parse_notification_without_id() {
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"text":"hi"}}"#;
        let req = parse_request(line).unwrap();
        assert!(req.id.is_none());
    }

    #[test]
    fn parse_invalid_json() {
        let result = parse_request("not json");
        assert!(result.is_err());
    }

    #[test]
    fn success_response_serde() {
        let resp = JsonRpcResponse::success(Some(json!(1)), json!({"ok": true}));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"result\""));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn error_response_serde() {
        let resp = JsonRpcResponse::error(Some(json!(1)), -32600, "Invalid Request");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("-32600"));
        assert!(json.contains("Invalid Request"));
        assert!(!json.contains("\"result\""));
    }

    #[test]
    fn method_not_found_response() {
        let resp = JsonRpcResponse::method_not_found(Some(json!(1)));
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
    }

    #[test]
    fn invalid_params_response() {
        let resp = JsonRpcResponse::invalid_params(Some(json!(1)), "missing field");
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "missing field");
    }

    #[test]
    fn notification_serde() {
        let notif = JsonRpcNotification::new("session/update", json!({"text": "hello"}));
        let json = serialize_notification(&notif);
        assert!(json.contains("session/update"));
        assert!(json.contains("hello"));
    }

    #[test]
    fn serialize_response_handles_all_cases() {
        let success = serialize_response(&JsonRpcResponse::success(None, json!(42)));
        assert!(success.contains("42"));

        let error = serialize_response(&JsonRpcResponse::internal_error(None, "boom"));
        assert!(error.contains("boom"));
    }

    #[test]
    fn roundtrip_request() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            method: "test/method".into(),
            params: Some(json!({"key": "val"})),
            id: Some(json!("req-1")),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.method, "test/method");
        assert_eq!(parsed.id, Some(json!("req-1")));
    }

    #[test]
    fn server_capabilities_has_required_fields() {
        let caps = server_capabilities();
        assert!(caps.get("protocolVersion").is_some());
        assert!(caps.get("serverInfo").is_some());
        assert!(caps.get("capabilities").is_some());
        assert_eq!(caps["capabilities"]["streaming"], true);
        assert_eq!(caps["capabilities"]["permissionFlow"], true);
    }

    #[tokio::test]
    async fn serve_stdio_initialize_method() {
        let runtime = test_runtime();

        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"id\":1}\n";
        let mut output = Vec::new();

        serve_stdio_io(runtime, &input[..], &mut output).await;

        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        assert_eq!(resp.id, Some(json!(1)));
        let result = resp.result.unwrap();
        assert!(result.get("protocolVersion").is_some());
    }

    #[tokio::test]
    async fn serve_stdio_unknown_method() {
        let runtime = test_runtime();

        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"unknown/method\",\"id\":2}\n";
        let mut output = Vec::new();

        serve_stdio_io(runtime, &input[..], &mut output).await;

        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn serve_stdio_parse_error() {
        let runtime = test_runtime();

        let input = b"not valid json\n";
        let mut output = Vec::new();

        serve_stdio_io(runtime, &input[..], &mut output).await;

        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32700);
    }

    #[tokio::test]
    async fn serve_stdio_empty_lines_skipped() {
        let runtime = test_runtime();

        let input = b"\n  \n{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"id\":3}\n\n";
        let mut output = Vec::new();

        serve_stdio_io(runtime, &input[..], &mut output).await;

        let output_str = String::from_utf8(output).unwrap();
        // Should only have one response line
        let lines: Vec<&str> = output_str.trim().lines().collect();
        assert_eq!(lines.len(), 1);
        let resp: JsonRpcResponse = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(resp.id, Some(json!(3)));
    }

    #[tokio::test]
    async fn serve_stdio_run_prompt_no_message() {
        let runtime = test_runtime();

        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"run_prompt\",\"params\":{},\"id\":4}\n";
        let mut output = Vec::new();

        serve_stdio_io(runtime, &input[..], &mut output).await;

        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32602);
    }

    #[tokio::test]
    async fn serve_stdio_multiple_requests() {
        let runtime = test_runtime();

        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"id\":1}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"unknown\",\"id\":2}\n",
        );
        let mut output = Vec::new();

        serve_stdio_io(runtime, input.as_bytes(), &mut output).await;

        let output_str = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = output_str.trim().lines().collect();
        assert_eq!(lines.len(), 2);

        let resp1: JsonRpcResponse = serde_json::from_str(lines[0]).unwrap();
        assert!(resp1.result.is_some());

        let resp2: JsonRpcResponse = serde_json::from_str(lines[1]).unwrap();
        assert!(resp2.error.is_some());
    }
}

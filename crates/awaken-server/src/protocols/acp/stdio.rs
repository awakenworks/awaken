//! JSON-RPC 2.0 stdio server for ACP protocol.
//!
//! Reads line-delimited JSON-RPC 2.0 requests from stdin, dispatches them,
//! and writes responses/notifications to stdout.
//!
//! Implements the ACP specification lifecycle:
//! - `initialize` — version negotiation and capability exchange
//! - `session/new` — create a new session with cwd and MCP servers
//! - `session/prompt` — send a prompt (ContentBlock[]) to the agent
//! - `session/cancel` — cancel an ongoing prompt turn
//! - `session/update` — forward tool call decisions from client
//!
//! Reference: <https://agentclientprotocol.com/protocol/transports>

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use awaken_contract::contract::message::Message;
use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};
use awaken_runtime::AgentRuntime;

use super::encoder::AcpEncoder;
use super::types::{
    AgentCapabilities, AgentInfo, ContentBlock, PROTOCOL_VERSION, PermissionOutcome,
    PermissionResponse,
};

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

    /// Resource not found error (-32002).
    pub fn resource_not_found(id: Option<Value>, message: impl Into<String>) -> Self {
        Self::error(id, -32002, message)
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

// ── Session state ───────────────────────────────────────────────────

/// Tracks an active session's state.
struct SessionState {
    #[allow(dead_code)]
    cwd: String,
    /// The agent ID bound to this session (defaults to "default").
    agent_id: String,
    /// Thread ID used for the runtime's conversation tracking.
    thread_id: String,
    /// Cancellation token for the active prompt turn.
    cancel_tx: Option<tokio::sync::watch::Sender<bool>>,
}

/// Shared session registry.
type Sessions = Arc<Mutex<HashMap<String, SessionState>>>;

// ── Stdio server ────────────────────────────────────────────────────

/// Spec-compliant `initialize` response.
fn initialize_response(client_version: u16) -> Value {
    let _ = client_version; // reserved for future version negotiation
    let negotiated = PROTOCOL_VERSION;

    let caps = AgentCapabilities::default();
    let info = AgentInfo {
        name: "awaken-acp".into(),
        title: Some("Awaken ACP Agent".into()),
        version: Some(env!("CARGO_PKG_VERSION").into()),
    };

    serde_json::json!({
        "protocolVersion": negotiated,
        "agentCapabilities": serde_json::to_value(&caps).unwrap_or(Value::Null),
        "agentInfo": serde_json::to_value(&info).unwrap_or(Value::Null),
        "authMethods": [],
    })
}

/// Write a single line to the given writer and flush.
async fn write_line<W: AsyncWriteExt + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

/// Generate a session ID in the format `sess_<alphanumeric>`.
fn generate_session_id() -> String {
    format!("sess_{}", uuid::Uuid::now_v7().simple())
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
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
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
                let client_version = request
                    .params
                    .as_ref()
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_u64)
                    .unwrap_or(PROTOCOL_VERSION as u64) as u16;
                let resp =
                    JsonRpcResponse::success(request.id, initialize_response(client_version));
                let _ = write_line(&mut output, &serialize_response(&resp)).await;
            }
            "session/new" => {
                handle_session_new(&sessions, request, &mut output).await;
            }
            "session/prompt" => {
                handle_session_prompt(runtime.clone(), &sessions, request, &mut output).await;
            }
            "session/cancel" => {
                handle_session_cancel(&sessions, request, &mut output).await;
            }
            "session/update" => {
                handle_session_update(&runtime, &sessions, request, &mut output).await;
            }
            // Keep backward compat for `run_prompt` during migration
            "run_prompt" => {
                handle_run_prompt_compat(runtime.clone(), &sessions, request, &mut output).await;
            }
            _ => {
                // Unrecognized notifications are silently ignored per spec
                if request.id.is_some() {
                    let resp = JsonRpcResponse::method_not_found(request.id);
                    let _ = write_line(&mut output, &serialize_response(&resp)).await;
                }
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

// ── Handlers ────────────────────────────────────────────────────────

/// Handle `session/new` — create a new session.
async fn handle_session_new<W: AsyncWriteExt + Unpin>(
    sessions: &Sessions,
    request: JsonRpcRequest,
    output: &mut W,
) {
    let params = request.params.unwrap_or(Value::Null);
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or(".")
        .to_string();
    let agent_id = params
        .get("agentId")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_string();

    let session_id = generate_session_id();
    let thread_id = uuid::Uuid::now_v7().to_string();

    sessions.lock().await.insert(
        session_id.clone(),
        SessionState {
            cwd,
            agent_id,
            thread_id,
            cancel_tx: None,
        },
    );

    let resp = JsonRpcResponse::success(request.id, serde_json::json!({ "sessionId": session_id }));
    let _ = write_line(output, &serialize_response(&resp)).await;
}

/// Handle `session/prompt` — send a prompt and stream events.
async fn handle_session_prompt<W: AsyncWriteExt + Unpin>(
    runtime: Arc<AgentRuntime>,
    sessions: &Sessions,
    request: JsonRpcRequest,
    output: &mut W,
) {
    let params = request.params.unwrap_or(Value::Null);

    let session_id = match params.get("sessionId").and_then(Value::as_str) {
        Some(id) => id.to_string(),
        None => {
            let resp = JsonRpcResponse::invalid_params(request.id, "sessionId is required");
            let _ = write_line(output, &serialize_response(&resp)).await;
            return;
        }
    };

    // Parse ContentBlock[] prompt
    let prompt_blocks: Vec<ContentBlock> = match params.get("prompt") {
        Some(prompt_val) => match serde_json::from_value(prompt_val.clone()) {
            Ok(blocks) => blocks,
            Err(e) => {
                let resp = JsonRpcResponse::invalid_params(
                    request.id,
                    format!("invalid prompt content blocks: {e}"),
                );
                let _ = write_line(output, &serialize_response(&resp)).await;
                return;
            }
        },
        None => {
            let resp = JsonRpcResponse::invalid_params(request.id, "prompt is required");
            let _ = write_line(output, &serialize_response(&resp)).await;
            return;
        }
    };

    // Extract text from content blocks
    let text = content_blocks_to_text(&prompt_blocks);
    if text.is_empty() {
        let resp = JsonRpcResponse::invalid_params(
            request.id,
            "prompt must contain at least one text content block",
        );
        let _ = write_line(output, &serialize_response(&resp)).await;
        return;
    }

    let (agent_id, thread_id) = {
        let sessions_guard = sessions.lock().await;
        match sessions_guard.get(&session_id) {
            Some(state) => (state.agent_id.clone(), state.thread_id.clone()),
            None => {
                let resp = JsonRpcResponse::resource_not_found(
                    request.id,
                    format!("session not found: {session_id}"),
                );
                let _ = write_line(output, &serialize_response(&resp)).await;
                return;
            }
        }
    };

    // Set up cancellation
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    {
        let mut sessions_guard = sessions.lock().await;
        if let Some(state) = sessions_guard.get_mut(&session_id) {
            state.cancel_tx = Some(cancel_tx);
        }
    }

    let messages = vec![Message::user(&text)];

    // Execute the run and stream events via a channel
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = crate::transport::channel_sink::ChannelEventSink::new(event_tx);
    let run_request =
        awaken_runtime::RunRequest::new(thread_id.clone(), messages).with_agent_id(agent_id);

    let rt = runtime.clone();
    let run_handle = tokio::spawn(async move {
        if let Err(e) = rt.run(run_request, Arc::new(sink)).await {
            tracing::warn!(error = %e, "stdio run failed");
        }
    });

    // Stream events as JSON-RPC notifications
    let mut encoder = AcpEncoder::new().with_session_id(&session_id);
    let req_id = request.id.clone();
    loop {
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Some(ev) => {
                        let acp_events = encoder.on_agent_event(&ev);
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
                    None => break, // channel closed, run finished
                }
            }
            _ = cancel_rx_changed(&cancel_rx) => {
                // Cancellation requested — drop the run handle
                run_handle.abort();
                break;
            }
        }
    }

    // Clean up cancellation token
    {
        let mut sessions_guard = sessions.lock().await;
        if let Some(state) = sessions_guard.get_mut(&session_id) {
            state.cancel_tx = None;
        }
    }

    let _ = run_handle.await;

    // Respond with stopReason
    let stop_reason = "end_turn"; // default if run completed normally
    let resp = JsonRpcResponse::success(req_id, serde_json::json!({ "stopReason": stop_reason }));
    let _ = write_line(output, &serialize_response(&resp)).await;
}

/// Wait for a cancellation signal on a watch channel.
async fn cancel_rx_changed(rx: &tokio::sync::watch::Receiver<bool>) {
    let mut rx = rx.clone();
    loop {
        if rx.changed().await.is_err() {
            // Sender dropped, no cancellation
            std::future::pending::<()>().await;
        }
        if *rx.borrow() {
            return;
        }
    }
}

/// Handle `session/cancel` — cancel an ongoing prompt turn.
async fn handle_session_cancel<W: AsyncWriteExt + Unpin>(
    sessions: &Sessions,
    request: JsonRpcRequest,
    output: &mut W,
) {
    let params = request.params.unwrap_or(Value::Null);
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("");

    if session_id.is_empty() {
        if let Some(id) = request.id {
            let resp = JsonRpcResponse::invalid_params(Some(id), "sessionId is required");
            let _ = write_line(output, &serialize_response(&resp)).await;
        }
        return;
    }

    let sessions_guard = sessions.lock().await;
    if let Some(state) = sessions_guard.get(session_id)
        && let Some(cancel_tx) = &state.cancel_tx
    {
        let _ = cancel_tx.send(true);
    }
    // session/cancel is a notification — no response unless id present
    if let Some(id) = request.id {
        let resp = JsonRpcResponse::success(Some(id), serde_json::json!(null));
        let _ = write_line(output, &serialize_response(&resp)).await;
    }
}

/// Handle `session/update` — forward tool call decisions from client.
async fn handle_session_update<W: AsyncWriteExt + Unpin>(
    runtime: &AgentRuntime,
    sessions: &Sessions,
    request: JsonRpcRequest,
    output: &mut W,
) {
    let params = request.params.unwrap_or(Value::Null);

    // Resolve thread_id from sessionId or direct threadId
    let thread_id = if let Some(session_id) = params.get("sessionId").and_then(Value::as_str) {
        let sessions_guard = sessions.lock().await;
        sessions_guard
            .get(session_id)
            .map(|s| s.thread_id.clone())
            .unwrap_or_default()
    } else {
        params
            .get("threadId")
            .or_else(|| params.get("thread_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };

    let tool_call_id = params
        .get("toolCallId")
        .or_else(|| params.get("tool_call_id"))
        .and_then(Value::as_str)
        .unwrap_or("");

    if thread_id.is_empty() || tool_call_id.is_empty() {
        if let Some(id) = request.id {
            let resp = JsonRpcResponse::invalid_params(
                Some(id),
                "sessionId (or threadId) and toolCallId are required",
            );
            let _ = write_line(output, &serialize_response(&resp)).await;
        }
        return;
    }

    // Parse permission response format (spec: outcome + optionId)
    let action = if let Some(outcome_val) = params.get("outcome") {
        match serde_json::from_value::<PermissionResponse>(params.clone()) {
            Ok(perm_resp) => match perm_resp.outcome {
                PermissionOutcome::Cancelled => ResumeDecisionAction::Cancel,
                PermissionOutcome::Selected => {
                    // Check if the selected option is a rejection
                    if let Some(ref opt_id) = perm_resp.option_id {
                        if opt_id.contains("reject") {
                            ResumeDecisionAction::Cancel
                        } else {
                            ResumeDecisionAction::Resume
                        }
                    } else {
                        ResumeDecisionAction::Resume
                    }
                }
            },
            Err(_) => {
                // Fallback: treat as string outcome
                match outcome_val.as_str() {
                    Some("cancelled") => ResumeDecisionAction::Cancel,
                    _ => ResumeDecisionAction::Resume,
                }
            }
        }
    } else {
        // Legacy format: action field
        match params.get("action").and_then(Value::as_str) {
            Some("cancel") => ResumeDecisionAction::Cancel,
            _ => ResumeDecisionAction::Resume,
        }
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

    let sent = runtime.send_decisions(&thread_id, vec![(tool_call_id.to_string(), resume)]);

    if let Some(id) = request.id {
        if sent {
            let resp = JsonRpcResponse::success(Some(id), serde_json::json!({"ok": true}));
            let _ = write_line(output, &serialize_response(&resp)).await;
        } else {
            let resp = JsonRpcResponse::internal_error(Some(id), "no active run for session");
            let _ = write_line(output, &serialize_response(&resp)).await;
        }
    }
}

/// Backward-compatible `run_prompt` handler that auto-creates a session.
async fn handle_run_prompt_compat<W: AsyncWriteExt + Unpin>(
    runtime: Arc<AgentRuntime>,
    sessions: &Sessions,
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

    // Auto-create session for backward compat
    let session_id = generate_session_id();
    sessions.lock().await.insert(
        session_id.clone(),
        SessionState {
            cwd: ".".into(),
            agent_id: agent_id.clone(),
            thread_id: thread_id.clone(),
            cancel_tx: None,
        },
    );

    let messages = vec![Message::user(text)];

    // ACK the request immediately with the run/thread identifiers
    let run_id = uuid::Uuid::now_v7().to_string();
    let resp = JsonRpcResponse::success(
        request.id,
        serde_json::json!({
            "runId": run_id,
            "threadId": thread_id,
            "sessionId": session_id,
        }),
    );
    let _ = write_line(output, &serialize_response(&resp)).await;

    // Execute the run and stream events via a channel
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = crate::transport::channel_sink::ChannelEventSink::new(event_tx);
    let run_request = awaken_runtime::RunRequest::new(thread_id, messages).with_agent_id(agent_id);

    let rt = runtime.clone();
    let run_handle = tokio::spawn(async move {
        if let Err(e) = rt.run(run_request, Arc::new(sink)).await {
            tracing::warn!(error = %e, "stdio run failed");
        }
    });

    // Stream events as JSON-RPC notifications
    let mut encoder = AcpEncoder::new().with_session_id(&session_id);
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

    let _ = run_handle.await;

    // Clean up session
    sessions.lock().await.remove(&session_id);
}

/// Convert content blocks to a single text string.
fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Stub resolver that always returns an error (no agents registered).
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
        let line = r#"{"jsonrpc":"2.0","method":"session/new","params":{"cwd":"/tmp"},"id":1}"#;
        let req = parse_request(line).unwrap();
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.method, "session/new");
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
    fn resource_not_found_response() {
        let resp = JsonRpcResponse::resource_not_found(Some(json!(1)), "session not found");
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32002);
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
    fn initialize_response_has_spec_fields() {
        let resp = initialize_response(1);
        assert_eq!(resp["protocolVersion"], PROTOCOL_VERSION);
        assert!(resp.get("agentCapabilities").is_some());
        assert!(resp.get("agentInfo").is_some());
        assert!(resp.get("authMethods").is_some());
        assert_eq!(resp["agentInfo"]["name"], "awaken-acp");
    }

    #[test]
    fn initialize_response_version_negotiation() {
        // Client sends version 1, we support 1 => echo 1
        let resp = initialize_response(1);
        assert_eq!(resp["protocolVersion"], 1);

        // Client sends version 99 (future), we still return our version
        let resp = initialize_response(99);
        assert_eq!(resp["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn generate_session_id_format() {
        let id = generate_session_id();
        assert!(id.starts_with("sess_"));
        assert!(id.len() > 5);
    }

    #[test]
    fn content_blocks_to_text_extracts_text_only() {
        let blocks = vec![
            ContentBlock::Text {
                text: "hello".into(),
            },
            ContentBlock::ResourceLink {
                uri: "file:///foo.rs".into(),
                name: "foo.rs".into(),
                description: None,
            },
            ContentBlock::Text {
                text: "world".into(),
            },
        ];
        assert_eq!(content_blocks_to_text(&blocks), "hello\nworld");
    }

    #[test]
    fn content_blocks_to_text_empty() {
        let blocks: Vec<ContentBlock> = vec![];
        assert_eq!(content_blocks_to_text(&blocks), "");
    }

    #[tokio::test]
    async fn serve_stdio_initialize_method() {
        let runtime = test_runtime();

        let input =
            b"{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{\"protocolVersion\":1},\"id\":1}\n";
        let mut output = Vec::new();

        serve_stdio_io(runtime, &input[..], &mut output).await;

        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        assert_eq!(resp.id, Some(json!(1)));
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        assert!(result.get("agentCapabilities").is_some());
        assert!(result.get("agentInfo").is_some());
    }

    #[tokio::test]
    async fn serve_stdio_session_new() {
        let runtime = test_runtime();

        let input =
            b"{\"jsonrpc\":\"2.0\",\"method\":\"session/new\",\"params\":{\"cwd\":\"/tmp\"},\"id\":1}\n";
        let mut output = Vec::new();

        serve_stdio_io(runtime, &input[..], &mut output).await;

        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert!(resp.result.is_some());
        let result = resp.result.unwrap();
        let session_id = result["sessionId"].as_str().unwrap();
        assert!(session_id.starts_with("sess_"));
    }

    #[tokio::test]
    async fn serve_stdio_session_prompt_requires_session() {
        let runtime = test_runtime();

        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"session/prompt\",\"params\":{\"prompt\":[{\"type\":\"text\",\"text\":\"hi\"}]},\"id\":1}\n";
        let mut output = Vec::new();

        serve_stdio_io(runtime, &input[..], &mut output).await;

        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32602); // invalid params: sessionId required
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

        let input = b"\n  \n{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{\"protocolVersion\":1},\"id\":3}\n\n";
        let mut output = Vec::new();

        serve_stdio_io(runtime, &input[..], &mut output).await;

        let output_str = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = output_str.trim().lines().collect();
        assert_eq!(lines.len(), 1);
        let resp: JsonRpcResponse = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(resp.id, Some(json!(3)));
    }

    #[tokio::test]
    async fn serve_stdio_run_prompt_compat_no_message() {
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
            "{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{\"protocolVersion\":1},\"id\":1}\n",
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

    #[tokio::test]
    async fn serve_stdio_unknown_notification_silently_ignored() {
        let runtime = test_runtime();

        // Notification (no id) with unknown method — should be silently ignored
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"_custom/something\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{\"protocolVersion\":1},\"id\":1}\n",
        );
        let mut output = Vec::new();

        serve_stdio_io(runtime, input.as_bytes(), &mut output).await;

        let output_str = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = output_str.trim().lines().collect();
        // Only the initialize response should be emitted
        assert_eq!(lines.len(), 1);
        let resp: JsonRpcResponse = serde_json::from_str(lines[0]).unwrap();
        assert!(resp.result.is_some());
    }

    #[tokio::test]
    async fn serve_stdio_session_prompt_invalid_session() {
        let runtime = test_runtime();

        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"sess_nonexistent\",\"prompt\":[{\"type\":\"text\",\"text\":\"hi\"}]},\"id\":1}\n";
        let mut output = Vec::new();

        serve_stdio_io(runtime, &input[..], &mut output).await;

        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32002); // resource not found
    }
}

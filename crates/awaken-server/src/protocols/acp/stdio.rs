//! JSON-RPC 2.0 stdio server for ACP protocol.
//!
//! Uses the official `agent-client-protocol-schema` types for all wire formats.
//! Reads line-delimited JSON-RPC 2.0 requests from stdin, dispatches them,
//! and writes responses/notifications to stdout.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use awaken_contract::contract::message::Message;
use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};
use awaken_runtime::AgentRuntime;

use super::encoder::{AcpEncoder, AcpOutput};
use super::types::{
    AgentCapabilities, ContentBlock, Implementation, InitializeResponse, NewSessionResponse,
    PromptResponse, ProtocolVersion, RequestPermissionOutcome, StopReason,
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
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: Some(result),
            error: None,
            id,
        }
    }

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

    pub fn method_not_found(id: Option<Value>) -> Self {
        Self::error(id, -32601, "Method not found")
    }

    pub fn invalid_params(id: Option<Value>, message: impl Into<String>) -> Self {
        Self::error(id, -32602, message)
    }

    pub fn internal_error(id: Option<Value>, message: impl Into<String>) -> Self {
        Self::error(id, -32603, message)
    }

    pub fn resource_not_found(id: Option<Value>, message: impl Into<String>) -> Self {
        Self::error(id, -32002, message)
    }
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.into(),
            params: Some(params),
        }
    }
}

pub fn parse_request(line: &str) -> Result<JsonRpcRequest, String> {
    serde_json::from_str(line).map_err(|e| format!("invalid JSON-RPC request: {e}"))
}

pub fn serialize_response(response: &JsonRpcResponse) -> String {
    serde_json::to_string(response).unwrap_or_else(|_| {
        r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"serialization error"},"id":null}"#
            .to_string()
    })
}

pub fn serialize_notification(notification: &JsonRpcNotification) -> String {
    serde_json::to_string(notification)
        .unwrap_or_else(|_| r#"{"jsonrpc":"2.0","method":"error","params":null}"#.to_string())
}

// ── Session state ───────────────────────────────────────────────────

struct SessionState {
    #[allow(dead_code)]
    cwd: String,
    agent_id: String,
    thread_id: String,
    cancel_tx: Option<tokio::sync::watch::Sender<bool>>,
}

type Sessions = Arc<Mutex<HashMap<String, SessionState>>>;

// ── Stdio server ────────────────────────────────────────────────────

fn build_initialize_response() -> Value {
    let resp = InitializeResponse::new(ProtocolVersion::V1)
        .agent_capabilities(AgentCapabilities::default())
        .agent_info(Implementation::new("awaken-acp", env!("CARGO_PKG_VERSION")));
    serde_json::to_value(&resp).unwrap_or(Value::Null)
}

async fn write_line<W: AsyncWriteExt + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

fn generate_session_id() -> String {
    format!("sess_{}", uuid::Uuid::now_v7().simple())
}

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
                let resp = JsonRpcResponse::success(request.id, build_initialize_response());
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
            "run_prompt" => {
                handle_run_prompt_compat(runtime.clone(), &sessions, request, &mut output).await;
            }
            _ => {
                if request.id.is_some() {
                    let resp = JsonRpcResponse::method_not_found(request.id);
                    let _ = write_line(&mut output, &serialize_response(&resp)).await;
                }
            }
        }
    }
}

pub async fn serve_stdio(runtime: Arc<AgentRuntime>) {
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_stdio_io(runtime, stdin, stdout).await;
}

// ── Handlers ────────────────────────────────────────────────────────

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

    let resp_value =
        serde_json::to_value(&NewSessionResponse::new(session_id.clone())).unwrap_or(Value::Null);
    let resp = JsonRpcResponse::success(request.id, resp_value);
    let _ = write_line(output, &serialize_response(&resp)).await;
}

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
        let guard = sessions.lock().await;
        match guard.get(&session_id) {
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

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    {
        let mut guard = sessions.lock().await;
        if let Some(state) = guard.get_mut(&session_id) {
            state.cancel_tx = Some(cancel_tx);
        }
    }

    let messages = vec![Message::user(&text)];
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = crate::transport::channel_sink::ChannelEventSink::new(event_tx);
    let run_request = awaken_runtime::RunRequest::new(thread_id, messages).with_agent_id(agent_id);

    let rt = runtime.clone();
    let run_handle = tokio::spawn(async move {
        if let Err(e) = rt.run(run_request, Arc::new(sink)).await {
            tracing::warn!(error = %e, "stdio run failed");
        }
    });

    let mut encoder = AcpEncoder::new().with_session_id(&session_id);
    let mut final_stop_reason = StopReason::EndTurn;

    loop {
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Some(ev) => {
                        for acp_out in encoder.on_agent_event(&ev) {
                            match acp_out {
                                AcpOutput::Notification(notif) => {
                                    if let Ok(params) = serde_json::to_value(&notif) {
                                        let n = JsonRpcNotification::new("session/update", params);
                                        let _ = write_line(output, &serialize_notification(&n)).await;
                                    }
                                }
                                AcpOutput::PermissionRequest(req) => {
                                    if let Ok(params) = serde_json::to_value(&req) {
                                        let n = JsonRpcNotification::new("session/request_permission", params);
                                        let _ = write_line(output, &serialize_notification(&n)).await;
                                    }
                                }
                                AcpOutput::Finished(reason) => {
                                    final_stop_reason = reason;
                                }
                                AcpOutput::Error { message, code } => {
                                    let err_val = serde_json::json!({"message": message, "code": code});
                                    let n = JsonRpcNotification::new("session/error", err_val);
                                    let _ = write_line(output, &serialize_notification(&n)).await;
                                }
                            }
                        }
                    }
                    None => break,
                }
            }
            _ = cancel_rx_changed(&cancel_rx) => {
                run_handle.abort();
                final_stop_reason = StopReason::Cancelled;
                break;
            }
        }
    }

    {
        let mut guard = sessions.lock().await;
        if let Some(state) = guard.get_mut(&session_id) {
            state.cancel_tx = None;
        }
    }

    let _ = run_handle.await;

    let prompt_resp = PromptResponse::new(final_stop_reason);
    let resp_value = serde_json::to_value(&prompt_resp).unwrap_or(Value::Null);
    let resp = JsonRpcResponse::success(request.id, resp_value);
    let _ = write_line(output, &serialize_response(&resp)).await;
}

async fn cancel_rx_changed(rx: &tokio::sync::watch::Receiver<bool>) {
    let mut rx = rx.clone();
    loop {
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        if *rx.borrow() {
            return;
        }
    }
}

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

    let guard = sessions.lock().await;
    if let Some(state) = guard.get(session_id)
        && let Some(cancel_tx) = &state.cancel_tx
    {
        let _ = cancel_tx.send(true);
    }

    if let Some(id) = request.id {
        let resp = JsonRpcResponse::success(Some(id), serde_json::json!(null));
        let _ = write_line(output, &serialize_response(&resp)).await;
    }
}

async fn handle_session_update<W: AsyncWriteExt + Unpin>(
    runtime: &AgentRuntime,
    sessions: &Sessions,
    request: JsonRpcRequest,
    output: &mut W,
) {
    let params = request.params.unwrap_or(Value::Null);

    let thread_id = if let Some(session_id) = params.get("sessionId").and_then(Value::as_str) {
        let guard = sessions.lock().await;
        guard
            .get(session_id)
            .map(|s| s.thread_id.clone())
            .unwrap_or_default()
    } else {
        params
            .get("threadId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };

    let tool_call_id = params
        .get("toolCallId")
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

    // Parse spec-compliant permission response or legacy format
    let action = if let Some(outcome) = params.get("outcome") {
        match serde_json::from_value::<RequestPermissionOutcome>(outcome.clone()) {
            Ok(RequestPermissionOutcome::Cancelled) => ResumeDecisionAction::Cancel,
            Ok(RequestPermissionOutcome::Selected(sel)) => {
                if sel.option_id.0.contains("reject") {
                    ResumeDecisionAction::Cancel
                } else {
                    ResumeDecisionAction::Resume
                }
            }
            _ => match outcome.as_str() {
                Some("cancelled") => ResumeDecisionAction::Cancel,
                _ => ResumeDecisionAction::Resume,
            },
        }
    } else {
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

    let messages = vec![Message::user(text)];
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = crate::transport::channel_sink::ChannelEventSink::new(event_tx);
    let run_request = awaken_runtime::RunRequest::new(thread_id, messages).with_agent_id(agent_id);

    let rt = runtime.clone();
    let run_handle = tokio::spawn(async move {
        if let Err(e) = rt.run(run_request, Arc::new(sink)).await {
            tracing::warn!(error = %e, "stdio run failed");
        }
    });

    let mut encoder = AcpEncoder::new().with_session_id(&session_id);
    while let Some(event) = event_rx.recv().await {
        for acp_out in encoder.on_agent_event(&event) {
            match acp_out {
                AcpOutput::Notification(notif) => {
                    if let Ok(params) = serde_json::to_value(&notif) {
                        let n = JsonRpcNotification::new("session/update", params);
                        let _ = write_line(output, &serialize_notification(&n)).await;
                    }
                }
                AcpOutput::PermissionRequest(req) => {
                    if let Ok(params) = serde_json::to_value(&req) {
                        let n = JsonRpcNotification::new("session/request_permission", params);
                        let _ = write_line(output, &serialize_notification(&n)).await;
                    }
                }
                AcpOutput::Finished(_) | AcpOutput::Error { .. } => {}
            }
        }
    }

    let _ = run_handle.await;
    sessions.lock().await.remove(&session_id);
}

fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(tc) => Some(tc.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(req.method, "session/new");
    }

    #[test]
    fn parse_invalid_json() {
        assert!(parse_request("not json").is_err());
    }

    #[test]
    fn initialize_response_has_spec_fields() {
        let resp = build_initialize_response();
        assert!(resp.get("protocolVersion").is_some());
        assert!(resp.get("agentCapabilities").is_some());
        assert!(resp.get("agentInfo").is_some());
    }

    #[test]
    fn generate_session_id_format() {
        let id = generate_session_id();
        assert!(id.starts_with("sess_"));
    }

    #[test]
    fn content_blocks_to_text_extracts_text_only() {
        let blocks = vec![ContentBlock::from("hello"), ContentBlock::from("world")];
        assert_eq!(content_blocks_to_text(&blocks), "hello\nworld");
    }

    #[tokio::test]
    async fn serve_stdio_initialize() {
        let runtime = test_runtime();
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{\"protocolVersion\":1},\"id\":1}\n";
        let mut output = Vec::new();
        serve_stdio_io(runtime, &input[..], &mut output).await;
        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[tokio::test]
    async fn serve_stdio_session_new() {
        let runtime = test_runtime();
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"session/new\",\"params\":{\"cwd\":\"/tmp\"},\"id\":1}\n";
        let mut output = Vec::new();
        serve_stdio_io(runtime, &input[..], &mut output).await;
        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        let result = resp.result.unwrap();
        assert!(result["sessionId"].as_str().unwrap().starts_with("sess_"));
    }

    #[tokio::test]
    async fn serve_stdio_unknown_method() {
        let runtime = test_runtime();
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"unknown\",\"id\":2}\n";
        let mut output = Vec::new();
        serve_stdio_io(runtime, &input[..], &mut output).await;
        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn serve_stdio_parse_error() {
        let runtime = test_runtime();
        let input = b"not json\n";
        let mut output = Vec::new();
        serve_stdio_io(runtime, &input[..], &mut output).await;
        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert_eq!(resp.error.unwrap().code, -32700);
    }

    #[tokio::test]
    async fn serve_stdio_run_prompt_compat_no_message() {
        let runtime = test_runtime();
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"run_prompt\",\"params\":{},\"id\":4}\n";
        let mut output = Vec::new();
        serve_stdio_io(runtime, &input[..], &mut output).await;
        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert_eq!(resp.error.unwrap().code, -32602);
    }

    #[tokio::test]
    async fn serve_stdio_session_prompt_requires_session() {
        let runtime = test_runtime();
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"session/prompt\",\"params\":{\"prompt\":[{\"type\":\"text\",\"text\":\"hi\"}]},\"id\":1}\n";
        let mut output = Vec::new();
        serve_stdio_io(runtime, &input[..], &mut output).await;
        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert_eq!(resp.error.unwrap().code, -32602);
    }

    #[tokio::test]
    async fn serve_stdio_session_prompt_invalid_session() {
        let runtime = test_runtime();
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"sess_bad\",\"prompt\":[{\"type\":\"text\",\"text\":\"hi\"}]},\"id\":1}\n";
        let mut output = Vec::new();
        serve_stdio_io(runtime, &input[..], &mut output).await;
        let output_str = String::from_utf8(output).unwrap();
        let resp: JsonRpcResponse = serde_json::from_str(output_str.trim()).unwrap();
        assert_eq!(resp.error.unwrap().code, -32002);
    }

    #[tokio::test]
    async fn serve_stdio_unknown_notification_silently_ignored() {
        let runtime = test_runtime();
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"_custom/something\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{},\"id\":1}\n",
        );
        let mut output = Vec::new();
        serve_stdio_io(runtime, input.as_bytes(), &mut output).await;
        let output_str = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = output_str.trim().lines().collect();
        assert_eq!(lines.len(), 1);
    }
}

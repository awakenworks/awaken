use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tirea_agentos::contracts::{Message, Role, RunRequest, ToolCallDecision};
use tirea_agentos::runtime::{AgentOs, RunLaunchSpec};
use tirea_contract::transport::Transcoder;
use tirea_protocol_acp::{AcpEncoder, AcpEvent};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

const PROTOCOL_VERSION: u64 = 1;
const SERVER_NAME: &str = "tirea-agentos";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 wire types
// ---------------------------------------------------------------------------

/// Incoming message: either a request (has `method`) or a response (has `result`/`error`).
#[derive(Debug, Deserialize)]
struct JsonRpcMessage {
    #[allow(dead_code)]
    jsonrpc: String,
    method: Option<String>,
    #[serde(default)]
    params: Value,
    id: Option<Value>,
    result: Option<Value>,
    error: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    result: Value,
    id: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcNotification {
    jsonrpc: &'static str,
    method: String,
    params: Value,
}

/// Agent→Client request (for permission flow).
#[derive(Debug, Serialize)]
struct JsonRpcAgentRequest {
    jsonrpc: &'static str,
    method: &'static str,
    params: Value,
    id: u64,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorResponse {
    jsonrpc: &'static str,
    error: JsonRpcErrorObject,
    id: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorObject {
    code: i64,
    message: String,
}

// ---------------------------------------------------------------------------
// ACP protocol params
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    #[allow(dead_code)]
    protocol_version: u64,
    #[allow(dead_code)]
    client_info: Option<Value>,
    #[allow(dead_code)]
    client_capabilities: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewSessionParams {
    #[serde(default)]
    _cwd: Option<String>,
    #[serde(default)]
    _mcp_servers: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptParams {
    session_id: String,
    prompt: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum ContentBlock {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

// Legacy params (kept for backward compat with old clients).
#[derive(Debug, Deserialize)]
struct LegacySessionStartParams {
    agent_id: String,
    thread_id: Option<String>,
    #[serde(default)]
    messages: Vec<SessionMessage>,
}

#[derive(Debug, Deserialize)]
struct SessionMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct LegacyPermissionResponseParams {
    tool_call_id: String,
    decision: String,
}

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

struct Session {
    session_id: String,
    agent_id: String,
    /// Active run ID for cancel support.
    run_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn make_response(id: Value, result: Value) -> String {
    serde_json::to_string(&JsonRpcResponse {
        jsonrpc: "2.0",
        result,
        id,
    })
    .expect("response is always serializable")
}

fn make_error_response(id: Value, code: i64, message: impl Into<String>) -> String {
    serde_json::to_string(&JsonRpcErrorResponse {
        jsonrpc: "2.0",
        error: JsonRpcErrorObject {
            code,
            message: message.into(),
        },
        id,
    })
    .expect("error response is always serializable")
}

fn make_notification(method: &str, params: Value) -> String {
    serde_json::to_string(&JsonRpcNotification {
        jsonrpc: "2.0",
        method: method.to_string(),
        params,
    })
    .expect("notification is always serializable")
}

fn make_agent_request(method: &'static str, params: Value, id: u64) -> String {
    serde_json::to_string(&JsonRpcAgentRequest {
        jsonrpc: "2.0",
        method,
        params,
        id,
    })
    .expect("agent request is always serializable")
}

fn wrap_session_update(session_id: &str, event: &AcpEvent) -> Option<String> {
    let value = serde_json::to_value(event).expect("AcpEvent is always serializable");
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    Some(make_notification(
        "session/update",
        json!({
            "sessionId": session_id,
            "update": params,
        }),
    ))
}

fn parse_role(s: &str) -> Role {
    match s {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    }
}

fn convert_messages(msgs: Vec<SessionMessage>) -> Vec<Message> {
    msgs.into_iter()
        .map(|m| Message {
            id: None,
            role: parse_role(&m.role),
            content: m.content,
            tool_calls: None,
            tool_call_id: None,
            visibility: Default::default(),
            metadata: None,
        })
        .collect()
}

fn content_blocks_to_messages(blocks: &[ContentBlock]) -> Vec<Message> {
    let text: String = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return vec![];
    }
    vec![Message {
        id: None,
        role: Role::User,
        content: text,
        tool_calls: None,
        tool_call_id: None,
        visibility: Default::default(),
        metadata: None,
    }]
}

fn map_permission_decision(tool_call_id: &str, decision: &str) -> ToolCallDecision {
    let ts = now_ms();
    match decision {
        "allow_once" | "allow_always" => {
            ToolCallDecision::resume(tool_call_id, json!({"approved": true}), ts)
        }
        _ => ToolCallDecision::cancel(
            tool_call_id,
            json!({"approved": false}),
            Some("rejected by user".to_string()),
            ts,
        ),
    }
}

fn map_permission_outcome(tool_call_id: &str, outcome: &Value) -> Option<ToolCallDecision> {
    let kind = outcome.get("kind")?.as_str()?;
    match kind {
        "selected" => {
            let option_kind = outcome
                .get("option")
                .and_then(|o| o.get("kind"))
                .and_then(|k| k.as_str())
                .unwrap_or("reject_once");
            Some(map_permission_decision(tool_call_id, option_kind))
        }
        "cancelled" => Some(ToolCallDecision::cancel(
            tool_call_id,
            json!({"approved": false}),
            Some("permission prompt cancelled".to_string()),
            now_ms(),
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Run the ACP stdio server over real stdin/stdout.
pub async fn serve_stdio(os: Arc<AgentOs>) {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    serve_io(os, stdin, stdout).await;
}

/// Core ACP JSON-RPC server loop over generic async I/O.
///
/// Implements the Agent Client Protocol: `initialize` → `session/new` →
/// `session/prompt` lifecycle with bidirectional request support for
/// `session/request_permission`.
pub async fn serve_io<R, W>(os: Arc<AgentOs>, input: R, output: W)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut output = output;
        while let Some(line) = writer_rx.recv().await {
            if let Err(e) = output.write_all(line.as_bytes()).await {
                error!("output write error: {e}");
                break;
            }
            if !line.ends_with('\n') {
                if let Err(e) = output.write_all(b"\n").await {
                    error!("output write error: {e}");
                    break;
                }
            }
            if let Err(e) = output.flush().await {
                error!("output flush error: {e}");
                break;
            }
        }
    });

    let os_for_decisions = os.clone();
    let mut initialized = false;
    let mut session: Option<Session> = None;
    // Pending agent→client permission requests: jsonrpc_id → (tool_call_id, run_id).
    let pending_permissions: Arc<Mutex<HashMap<u64, (String, String)>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let next_request_id = Arc::new(AtomicU64::new(1));

    let mut reader = BufReader::new(input);
    let mut line_buf = String::new();

    loop {
        line_buf.clear();
        match reader.read_line(&mut line_buf).await {
            Ok(0) => {
                info!("input EOF — shutting down");
                break;
            }
            Ok(_) => {}
            Err(e) => {
                error!("input read error: {e}");
                break;
            }
        }

        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }

        let msg: JsonRpcMessage = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(e) => {
                warn!("invalid JSON-RPC: {e}");
                let _ = writer_tx.send(make_error_response(
                    Value::Null,
                    -32700,
                    format!("parse error: {e}"),
                ));
                continue;
            }
        };

        // --- Handle responses to agent→client requests (permission) ---
        if msg.method.is_none() && (msg.result.is_some() || msg.error.is_some()) {
            if let Some(id) = &msg.id {
                if let Some(rid) = id.as_u64() {
                    let entry = pending_permissions.lock().await.remove(&rid);
                    if let Some((tcid, run_id)) = entry {
                        let decision = if let Some(result) = &msg.result {
                            let outcome = result.get("outcome").cloned().unwrap_or(Value::Null);
                            map_permission_outcome(&tcid, &outcome)
                        } else {
                            Some(ToolCallDecision::cancel(
                                &tcid,
                                json!({"approved": false}),
                                Some("permission request errored".to_string()),
                                now_ms(),
                            ))
                        };
                        if let Some(d) = decision {
                            if os_for_decisions
                                .forward_decisions_by_run_id(&run_id, &[d])
                                .await
                                .is_none()
                            {
                                warn!("decision forward failed for run {run_id}");
                            }
                        }
                    }
                }
            }
            continue;
        }

        // --- Handle requests/notifications ---
        let method = match &msg.method {
            Some(m) => m.as_str(),
            None => continue,
        };

        debug!("recv method={method}");

        match method {
            // =============================================================
            // initialize
            // =============================================================
            "initialize" => {
                let _params: InitializeParams = match serde_json::from_value(msg.params) {
                    Ok(p) => p,
                    Err(e) => {
                        let id = msg.id.unwrap_or(Value::Null);
                        let _ = writer_tx.send(make_error_response(
                            id,
                            -32602,
                            format!("invalid params: {e}"),
                        ));
                        continue;
                    }
                };

                initialized = true;
                let id = msg.id.unwrap_or(Value::Null);
                let _ = writer_tx.send(make_response(
                    id,
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "agentInfo": {
                            "name": SERVER_NAME,
                            "version": SERVER_VERSION,
                        },
                        "agentCapabilities": {
                            "loadSession": false,
                            "promptCapabilities": {
                                "image": false,
                                "audio": false,
                                "embeddedContext": false,
                            },
                        },
                        "authMethods": [],
                    }),
                ));
                info!("initialized");
            }

            // =============================================================
            // session/new — create a session
            // =============================================================
            "session/new" => {
                if !initialized {
                    let id = msg.id.unwrap_or(Value::Null);
                    let _ = writer_tx.send(make_error_response(id, -32002, "not initialized"));
                    continue;
                }

                let _params: NewSessionParams = match serde_json::from_value(msg.params) {
                    Ok(p) => p,
                    Err(e) => {
                        let id = msg.id.unwrap_or(Value::Null);
                        let _ = writer_tx.send(make_error_response(
                            id,
                            -32602,
                            format!("invalid params: {e}"),
                        ));
                        continue;
                    }
                };

                let session_id = format!("sess_{}", uuid::Uuid::now_v7());
                // Use first available agent as default.
                let agent_id = os
                    .agent_ids()
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "default".to_string());

                session = Some(Session {
                    session_id: session_id.clone(),
                    agent_id,
                    run_id: None,
                });

                let id = msg.id.unwrap_or(Value::Null);
                let _ = writer_tx.send(make_response(id, json!({"sessionId": session_id})));
                info!(session_id = %session_id, "session created");
            }

            // =============================================================
            // session/prompt — send user prompt, stream updates, return stop reason
            // =============================================================
            "session/prompt" => {
                if !initialized {
                    let id = msg.id.unwrap_or(Value::Null);
                    let _ = writer_tx.send(make_error_response(id, -32002, "not initialized"));
                    continue;
                }

                let params: PromptParams = match serde_json::from_value(msg.params) {
                    Ok(p) => p,
                    Err(e) => {
                        let id = msg.id.unwrap_or(Value::Null);
                        let _ = writer_tx.send(make_error_response(
                            id,
                            -32602,
                            format!("invalid params: {e}"),
                        ));
                        continue;
                    }
                };

                let sess = match &session {
                    Some(s) if s.session_id == params.session_id => s,
                    _ => {
                        let id = msg.id.unwrap_or(Value::Null);
                        let _ = writer_tx.send(make_error_response(
                            id,
                            -32002,
                            format!("unknown session: {}", params.session_id),
                        ));
                        continue;
                    }
                };

                let agent_id = sess.agent_id.clone();
                let session_id = sess.session_id.clone();

                let resolved = match os.resolve(&agent_id) {
                    Ok(r) => r,
                    Err(e) => {
                        let id = msg.id.unwrap_or(Value::Null);
                        let _ = writer_tx.send(make_error_response(
                            id,
                            -32001,
                            format!("resolve error: {e}"),
                        ));
                        continue;
                    }
                };

                let messages = content_blocks_to_messages(&params.prompt);
                let run_request = RunRequest {
                    agent_id: agent_id.clone(),
                    thread_id: Some(session_id.clone()),
                    run_id: None,
                    parent_run_id: None,
                    parent_thread_id: None,
                    resource_id: None,
                    origin: Default::default(),
                    state: None,
                    messages,
                    initial_decisions: vec![],
                    source_mailbox_entry_id: None,
                };

                let run = match os
                    .start_active_run_with_spec(
                        &agent_id,
                        run_request,
                        resolved,
                        RunLaunchSpec::DURABLE,
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        let id = msg.id.unwrap_or(Value::Null);
                        let _ = writer_tx.send(make_error_response(
                            id,
                            -32002,
                            format!("run error: {e}"),
                        ));
                        continue;
                    }
                };

                let run_id = run.run_id.clone();
                info!(session_id = %session_id, run_id = %run_id, "prompt started");

                if let Some(s) = &mut session {
                    s.run_id = Some(run_id.clone());
                }

                // Spawn event pump. When done, sends PromptResponse.
                let pump_writer = writer_tx.clone();
                let prompt_id = msg.id.clone().unwrap_or(Value::Null);
                let pump_session_id = session_id.clone();
                let pump_permissions = pending_permissions.clone();
                let pump_next_id = next_request_id.clone();
                let pump_run_id = run_id;
                tokio::spawn(async move {
                    let mut encoder = AcpEncoder::new();
                    let mut events = run.events;
                    let mut stop_reason = "end_turn".to_string();
                    while let Some(ev) = events.next().await {
                        let acp_events = encoder.transcode(&ev);
                        for acp_ev in &acp_events {
                            match acp_ev {
                                AcpEvent::RequestPermission(perm) => {
                                    let rid = pump_next_id.fetch_add(1, Ordering::Relaxed);
                                    pump_permissions.lock().await.insert(
                                        rid,
                                        (perm.tool_call_id.clone(), pump_run_id.clone()),
                                    );
                                    let line = make_agent_request(
                                        "session/request_permission",
                                        json!({
                                            "sessionId": pump_session_id,
                                            "toolCall": {
                                                "id": perm.tool_call_id,
                                                "name": perm.tool_name,
                                                "args": perm.tool_args,
                                            },
                                            "options": perm.options,
                                        }),
                                        rid,
                                    );
                                    if pump_writer.send(line).is_err() {
                                        return;
                                    }
                                }
                                AcpEvent::SessionUpdate(update) => {
                                    if let Some(finished) = &update.finished {
                                        stop_reason = serde_json::to_value(&finished.stop_reason)
                                            .ok()
                                            .and_then(|v| v.as_str().map(String::from))
                                            .unwrap_or_else(|| "end_turn".to_string());
                                    }
                                    if let Some(line) =
                                        wrap_session_update(&pump_session_id, acp_ev)
                                    {
                                        if pump_writer.send(line).is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let _ = pump_writer
                        .send(make_response(prompt_id, json!({"stopReason": stop_reason})));
                });
            }

            // =============================================================
            // session/cancel — notification
            // =============================================================
            "session/cancel" => {
                if let Some(s) = &session {
                    if let Some(run_id) = &s.run_id {
                        let cancelled = os.cancel_active_run_by_id(run_id).await;
                        info!(run_id = %run_id, cancelled, "session/cancel");
                    }
                }
            }

            // =============================================================
            // Legacy: session/start (old protocol)
            // =============================================================
            "session/start" => {
                let params: LegacySessionStartParams = match serde_json::from_value(msg.params) {
                    Ok(p) => p,
                    Err(e) => {
                        let id = msg.id.unwrap_or(Value::Null);
                        let _ = writer_tx.send(make_error_response(
                            id,
                            -32602,
                            format!("invalid params: {e}"),
                        ));
                        continue;
                    }
                };

                let agent_id = params.agent_id.clone();
                let resolved = match os.resolve(&agent_id) {
                    Ok(r) => r,
                    Err(e) => {
                        let id = msg.id.unwrap_or(Value::Null);
                        let _ = writer_tx.send(make_error_response(
                            id,
                            -32001,
                            format!("resolve error: {e}"),
                        ));
                        continue;
                    }
                };

                let run_request = RunRequest {
                    agent_id: agent_id.clone(),
                    thread_id: params.thread_id,
                    run_id: None,
                    parent_run_id: None,
                    parent_thread_id: None,
                    resource_id: None,
                    origin: Default::default(),
                    state: None,
                    messages: convert_messages(params.messages),
                    initial_decisions: vec![],
                    source_mailbox_entry_id: None,
                };

                let run = match os
                    .start_active_run_with_spec(
                        &agent_id,
                        run_request,
                        resolved,
                        RunLaunchSpec::DURABLE,
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        let id = msg.id.unwrap_or(Value::Null);
                        let _ = writer_tx.send(make_error_response(
                            id,
                            -32002,
                            format!("run error: {e}"),
                        ));
                        continue;
                    }
                };

                let run_id = run.run_id.clone();
                session = Some(Session {
                    session_id: format!("legacy_{run_id}"),
                    agent_id: agent_id.clone(),
                    run_id: Some(run_id.clone()),
                });

                let pump_writer = writer_tx.clone();
                let pump_permissions = pending_permissions.clone();
                let pump_next_id = next_request_id.clone();
                let pump_run_id = run_id;
                tokio::spawn(async move {
                    let mut encoder = AcpEncoder::new();
                    let mut events = run.events;
                    while let Some(ev) = events.next().await {
                        let acp_events = encoder.transcode(&ev);
                        for acp_ev in &acp_events {
                            match acp_ev {
                                AcpEvent::RequestPermission(perm) => {
                                    let rid = pump_next_id.fetch_add(1, Ordering::Relaxed);
                                    pump_permissions.lock().await.insert(
                                        rid,
                                        (perm.tool_call_id.clone(), pump_run_id.clone()),
                                    );
                                    let line = make_agent_request(
                                        "session/request_permission",
                                        json!({
                                            "sessionId": "",
                                            "toolCall": {
                                                "id": perm.tool_call_id,
                                                "name": perm.tool_name,
                                                "args": perm.tool_args,
                                            },
                                            "options": perm.options,
                                        }),
                                        rid,
                                    );
                                    if pump_writer.send(line).is_err() {
                                        return;
                                    }
                                }
                                AcpEvent::SessionUpdate(_) => {
                                    let value = serde_json::to_value(acp_ev)
                                        .expect("AcpEvent is always serializable");
                                    let method = value
                                        .get("method")
                                        .and_then(|m| m.as_str())
                                        .unwrap_or("session/update")
                                        .to_string();
                                    let params =
                                        value.get("params").cloned().unwrap_or(Value::Null);
                                    let line = make_notification(&method, params);
                                    if pump_writer.send(line).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    }
                });
            }

            // =============================================================
            // Legacy: session/permission_response (old protocol)
            // =============================================================
            "session/permission_response" => {
                let params: LegacyPermissionResponseParams =
                    match serde_json::from_value(msg.params) {
                        Ok(p) => p,
                        Err(e) => {
                            let id = msg.id.unwrap_or(Value::Null);
                            let _ = writer_tx.send(make_error_response(
                                id,
                                -32602,
                                format!("invalid params: {e}"),
                            ));
                            continue;
                        }
                    };

                let decision = map_permission_decision(&params.tool_call_id, &params.decision);
                if let Some(s) = &session {
                    if let Some(run_id) = &s.run_id {
                        if os_for_decisions
                            .forward_decisions_by_run_id(run_id, &[decision])
                            .await
                            .is_none()
                        {
                            warn!("decision forward failed for run {run_id}");
                        }
                    }
                } else {
                    warn!("permission_response received but no active session");
                }
            }

            _ => {
                let id = msg.id.unwrap_or(Value::Null);
                let _ = writer_tx.send(make_error_response(
                    id,
                    -32601,
                    format!("method not found: {method}"),
                ));
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Initialize --

    #[test]
    fn initialize_response_contains_protocol_version_and_agent_info() {
        let result = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "agentInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
            "agentCapabilities": {
                "loadSession": false,
                "promptCapabilities": {"image": false, "audio": false, "embeddedContext": false},
            },
            "authMethods": [],
        });
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["agentInfo"]["name"], "tirea-agentos");
        assert!(!result["agentInfo"]["version"].as_str().unwrap().is_empty());
        assert_eq!(result["agentCapabilities"]["loadSession"], false);
        assert!(result["authMethods"].as_array().unwrap().is_empty());
    }

    #[test]
    fn parse_initialize_params() {
        let raw = json!({
            "protocolVersion": 1,
            "clientInfo": {"name": "Zed", "version": "0.200"},
            "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": true}
        });
        let params: InitializeParams = serde_json::from_value(raw).unwrap();
        assert_eq!(params.protocol_version, 1);
    }

    // -- session/new --

    #[test]
    fn parse_new_session_params() {
        let raw = json!({"cwd": "/home/user/project", "mcpServers": []});
        let params: NewSessionParams = serde_json::from_value(raw).unwrap();
        assert_eq!(params._cwd.as_deref(), Some("/home/user/project"));
        assert!(params._mcp_servers.is_empty());
    }

    #[test]
    fn parse_new_session_params_minimal() {
        let raw = json!({});
        let params: NewSessionParams = serde_json::from_value(raw).unwrap();
        assert!(params._cwd.is_none());
    }

    // -- session/prompt --

    #[test]
    fn parse_prompt_params_text() {
        let raw = json!({
            "sessionId": "sess_abc",
            "prompt": [{"kind": "text", "text": "hello world"}]
        });
        let params: PromptParams = serde_json::from_value(raw).unwrap();
        assert_eq!(params.session_id, "sess_abc");
        assert_eq!(params.prompt.len(), 1);
        match &params.prompt[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hello world"),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn content_blocks_to_messages_extracts_text() {
        let blocks = vec![
            ContentBlock::Text {
                text: "hello".to_string(),
            },
            ContentBlock::Text {
                text: "world".to_string(),
            },
        ];
        let msgs = content_blocks_to_messages(&blocks);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "hello\nworld");
        assert!(matches!(msgs[0].role, Role::User));
    }

    #[test]
    fn content_blocks_to_messages_empty_returns_empty() {
        let blocks: Vec<ContentBlock> = vec![];
        assert!(content_blocks_to_messages(&blocks).is_empty());
    }

    // -- JSON-RPC response helpers --

    #[test]
    fn make_response_produces_valid_jsonrpc() {
        let line = make_response(json!(1), json!({"ok": true}));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["result"]["ok"], true);
        assert!(parsed.get("error").is_none());
    }

    #[test]
    fn make_agent_request_produces_valid_jsonrpc() {
        let line = make_agent_request("session/request_permission", json!({"foo": 1}), 42);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "session/request_permission");
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["params"]["foo"], 1);
    }

    // -- Bidirectional message parsing --

    #[test]
    fn parse_jsonrpc_request_message() {
        let raw =
            json!({"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":1},"id":0});
        let msg: JsonRpcMessage = serde_json::from_value(raw).unwrap();
        assert_eq!(msg.method.as_deref(), Some("initialize"));
        assert!(msg.result.is_none());
    }

    #[test]
    fn parse_jsonrpc_response_message() {
        let raw = json!({"jsonrpc":"2.0","result":{"outcome":{"kind":"selected","option":{"kind":"allow_once"}}},"id":42});
        let msg: JsonRpcMessage = serde_json::from_value(raw).unwrap();
        assert!(msg.method.is_none());
        assert!(msg.result.is_some());
        assert_eq!(msg.id, Some(json!(42)));
    }

    // -- Permission outcome mapping --

    #[test]
    fn map_permission_outcome_selected_allow() {
        let outcome = json!({"kind": "selected", "option": {"kind": "allow_once"}});
        let d = map_permission_outcome("fc_1", &outcome).unwrap();
        assert_eq!(d.target_id, "fc_1");
        assert!(matches!(
            d.resume.action,
            tirea_contract::io::ResumeDecisionAction::Resume
        ));
    }

    #[test]
    fn map_permission_outcome_selected_reject() {
        let outcome = json!({"kind": "selected", "option": {"kind": "reject_once"}});
        let d = map_permission_outcome("fc_2", &outcome).unwrap();
        assert!(matches!(
            d.resume.action,
            tirea_contract::io::ResumeDecisionAction::Cancel
        ));
    }

    #[test]
    fn map_permission_outcome_cancelled() {
        let outcome = json!({"kind": "cancelled"});
        let d = map_permission_outcome("fc_3", &outcome).unwrap();
        assert!(matches!(
            d.resume.action,
            tirea_contract::io::ResumeDecisionAction::Cancel
        ));
    }

    // -- Legacy compat --

    #[test]
    fn parse_legacy_session_start() {
        let raw = json!({"agent_id": "default", "messages": [{"role": "user", "content": "hi"}]});
        let params: LegacySessionStartParams = serde_json::from_value(raw).unwrap();
        assert_eq!(params.agent_id, "default");
    }

    #[test]
    fn parse_legacy_permission_response() {
        let raw = json!({"tool_call_id": "fc_1", "decision": "allow_once"});
        let params: LegacyPermissionResponseParams = serde_json::from_value(raw).unwrap();
        assert_eq!(params.tool_call_id, "fc_1");
        assert_eq!(params.decision, "allow_once");
    }

    // -- wrap_session_update --

    #[test]
    fn wrap_session_update_includes_session_id() {
        let event = AcpEvent::agent_message("hi");
        let line = wrap_session_update("sess_123", &event).unwrap();
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["method"], "session/update");
        assert_eq!(parsed["params"]["sessionId"], "sess_123");
        assert!(parsed["params"]["update"].is_object());
    }

    // -- Existing helpers --

    #[test]
    fn convert_messages_maps_roles() {
        let msgs = vec![
            SessionMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            },
            SessionMessage {
                role: "assistant".to_string(),
                content: "hey".to_string(),
            },
            SessionMessage {
                role: "system".to_string(),
                content: "sys".to_string(),
            },
            SessionMessage {
                role: "tool".to_string(),
                content: "res".to_string(),
            },
        ];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 4);
        assert!(matches!(converted[0].role, Role::User));
        assert!(matches!(converted[1].role, Role::Assistant));
        assert!(matches!(converted[2].role, Role::System));
        assert!(matches!(converted[3].role, Role::Tool));
    }

    #[test]
    fn error_response_format() {
        let line = make_error_response(json!(99), -32601, "method not found: foo/bar");
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["error"]["code"], -32601);
        assert_eq!(parsed["id"], 99);
    }

    #[test]
    fn permission_decision_allow_maps_to_resume() {
        let d = map_permission_decision("fc_1", "allow_once");
        assert!(matches!(
            d.resume.action,
            tirea_contract::io::ResumeDecisionAction::Resume
        ));
    }

    #[test]
    fn permission_decision_reject_maps_to_cancel() {
        let d = map_permission_decision("fc_1", "reject_once");
        assert!(matches!(
            d.resume.action,
            tirea_contract::io::ResumeDecisionAction::Cancel
        ));
    }
}

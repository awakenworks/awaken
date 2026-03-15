use std::sync::Arc;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tirea_agentos::contracts::{Message, Role, RunRequest, ToolCallDecision};
use tirea_agentos::runtime::AgentOs;
use tirea_contract::transport::Transcoder;
use tirea_protocol_acp::{AcpEncoder, AcpEvent};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Value,
    id: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcNotification {
    jsonrpc: &'static str,
    method: String,
    params: Value,
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
// session/start params
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SessionStartParams {
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

// ---------------------------------------------------------------------------
// session/permission_response params
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PermissionResponseParams {
    tool_call_id: String,
    decision: String,
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

fn wrap_acp_event(event: &AcpEvent) -> String {
    let value = serde_json::to_value(event).expect("AcpEvent is always serializable");
    let method = value
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("session/update")
        .to_string();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    let notification = JsonRpcNotification {
        jsonrpc: "2.0",
        method,
        params,
    };
    serde_json::to_string(&notification).expect("notification is always serializable")
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

fn map_permission_decision(tool_call_id: &str, decision: &str) -> ToolCallDecision {
    let ts = now_ms();
    match decision {
        "allow_once" | "allow_always" => {
            ToolCallDecision::resume(tool_call_id, serde_json::json!({"approved": true}), ts)
        }
        _ => ToolCallDecision::cancel(
            tool_call_id,
            serde_json::json!({"approved": false}),
            Some("rejected by user".to_string()),
            ts,
        ),
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the ACP stdio server: read JSON-RPC requests from stdin, write
/// JSON-RPC notifications to stdout, logs to stderr.
pub async fn serve_stdio(os: Arc<AgentOs>) {
    // Stdout writer task: serialises lines from mpsc channel.
    let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = writer_rx.recv().await {
            if let Err(e) = stdout.write_all(line.as_bytes()).await {
                error!("stdout write error: {e}");
                break;
            }
            if !line.ends_with('\n') {
                if let Err(e) = stdout.write_all(b"\n").await {
                    error!("stdout write error: {e}");
                    break;
                }
            }
            if let Err(e) = stdout.flush().await {
                error!("stdout flush error: {e}");
                break;
            }
        }
    });

    // Active session state: at most one session at a time (stdio is single-session).
    let mut active_decision_tx: Option<tokio::sync::mpsc::UnboundedSender<ToolCallDecision>> = None;

    // Stdin reader loop.
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line_buf = String::new();

    loop {
        line_buf.clear();
        match reader.read_line(&mut line_buf).await {
            Ok(0) => {
                info!("stdin EOF — shutting down");
                break;
            }
            Ok(_) => {}
            Err(e) => {
                error!("stdin read error: {e}");
                break;
            }
        }

        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                warn!("invalid JSON-RPC: {e}");
                let err_line =
                    make_error_response(Value::Null, -32700, format!("parse error: {e}"));
                let _ = writer_tx.send(err_line);
                continue;
            }
        };

        debug!("recv method={}", request.method);

        match request.method.as_str() {
            "session/start" => {
                let params: SessionStartParams = match serde_json::from_value(request.params) {
                    Ok(p) => p,
                    Err(e) => {
                        let id = request.id.unwrap_or(Value::Null);
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
                        let id = request.id.unwrap_or(Value::Null);
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
                    .start_active_run_with_persistence(
                        &agent_id,
                        run_request,
                        resolved,
                        true,
                        false,
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        let id = request.id.unwrap_or(Value::Null);
                        let _ = writer_tx.send(make_error_response(
                            id,
                            -32002,
                            format!("run error: {e}"),
                        ));
                        continue;
                    }
                };

                info!(
                    thread_id = %run.thread_id,
                    run_id = %run.run_id,
                    "session started"
                );

                active_decision_tx = Some(run.decision_tx.clone());

                // Spawn event pump.
                let pump_writer = writer_tx.clone();
                tokio::spawn(async move {
                    let mut encoder = AcpEncoder::new();
                    let mut events = run.events;
                    while let Some(ev) = events.next().await {
                        let acp_events = encoder.transcode(&ev);
                        for acp_ev in &acp_events {
                            let line = wrap_acp_event(acp_ev);
                            if pump_writer.send(line).is_err() {
                                return;
                            }
                        }
                    }
                });
            }

            "session/permission_response" => {
                let params: PermissionResponseParams = match serde_json::from_value(request.params)
                {
                    Ok(p) => p,
                    Err(e) => {
                        let id = request.id.unwrap_or(Value::Null);
                        let _ = writer_tx.send(make_error_response(
                            id,
                            -32602,
                            format!("invalid params: {e}"),
                        ));
                        continue;
                    }
                };

                let decision = map_permission_decision(&params.tool_call_id, &params.decision);
                if let Some(tx) = &active_decision_tx {
                    if let Err(e) = tx.send(decision) {
                        warn!("decision send failed (run may have ended): {e}");
                    }
                } else {
                    warn!("permission_response received but no active session");
                }
            }

            _ => {
                let id = request.id.unwrap_or(Value::Null);
                let _ = writer_tx.send(make_error_response(
                    id,
                    -32601,
                    format!("method not found: {}", request.method),
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
    use serde_json::json;
    use tirea_contract::io::ResumeDecisionAction;

    #[test]
    fn parse_session_start_request() {
        let raw = json!({
            "jsonrpc": "2.0",
            "method": "session/start",
            "params": {
                "agent_id": "default",
                "messages": [{"role": "user", "content": "hello"}]
            },
            "id": 1
        });
        let req: JsonRpcRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.method, "session/start");
        let params: SessionStartParams = serde_json::from_value(req.params).unwrap();
        assert_eq!(params.agent_id, "default");
        assert_eq!(params.messages.len(), 1);
        assert_eq!(params.messages[0].role, "user");
        assert_eq!(params.messages[0].content, "hello");
    }

    #[test]
    fn parse_session_start_with_thread_id() {
        let raw = json!({
            "jsonrpc": "2.0",
            "method": "session/start",
            "params": {
                "agent_id": "my-agent",
                "thread_id": "thread-42",
                "messages": []
            },
            "id": 2
        });
        let req: JsonRpcRequest = serde_json::from_value(raw).unwrap();
        let params: SessionStartParams = serde_json::from_value(req.params).unwrap();
        assert_eq!(params.thread_id.as_deref(), Some("thread-42"));
    }

    #[test]
    fn parse_permission_response() {
        let raw = json!({
            "jsonrpc": "2.0",
            "method": "session/permission_response",
            "params": {
                "tool_call_id": "fc_123",
                "decision": "allow_once"
            }
        });
        let req: JsonRpcRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.method, "session/permission_response");
        let params: PermissionResponseParams = serde_json::from_value(req.params).unwrap();
        assert_eq!(params.tool_call_id, "fc_123");
        assert_eq!(params.decision, "allow_once");
    }

    #[test]
    fn wrap_acp_event_as_jsonrpc_notification() {
        let event = AcpEvent::agent_message("hello world");
        let line = wrap_acp_event(&event);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "session/update");
        assert_eq!(parsed["params"]["agentMessageChunk"], "hello world");
        // Notifications have no id field.
        assert!(parsed.get("id").is_none());
    }

    #[test]
    fn wrap_acp_request_permission_event() {
        let event = AcpEvent::request_permission("fc_1", "bash", json!({"cmd": "ls"}));
        let line = wrap_acp_event(&event);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "session/request_permission");
        assert_eq!(parsed["params"]["toolCallId"], "fc_1");
        assert_eq!(parsed["params"]["toolName"], "bash");
    }

    #[test]
    fn wrap_acp_finished_event() {
        let event = AcpEvent::finished(tirea_protocol_acp::StopReason::EndTurn);
        let line = wrap_acp_event(&event);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["method"], "session/update");
        assert_eq!(parsed["params"]["finished"]["stopReason"], "end_turn");
    }

    #[test]
    fn permission_response_allow_once_maps_to_resume() {
        let d = map_permission_decision("fc_1", "allow_once");
        assert_eq!(d.target_id, "fc_1");
        assert!(matches!(d.resume.action, ResumeDecisionAction::Resume));
        assert_eq!(d.resume.result, json!({"approved": true}));
    }

    #[test]
    fn permission_response_allow_always_maps_to_resume() {
        let d = map_permission_decision("fc_2", "allow_always");
        assert!(matches!(d.resume.action, ResumeDecisionAction::Resume));
    }

    #[test]
    fn permission_response_reject_once_maps_to_cancel() {
        let d = map_permission_decision("fc_3", "reject_once");
        assert_eq!(d.target_id, "fc_3");
        assert!(matches!(d.resume.action, ResumeDecisionAction::Cancel));
        assert_eq!(d.resume.result, json!({"approved": false}));
        assert_eq!(d.resume.reason.as_deref(), Some("rejected by user"));
    }

    #[test]
    fn permission_response_reject_always_maps_to_cancel() {
        let d = map_permission_decision("fc_4", "reject_always");
        assert!(matches!(d.resume.action, ResumeDecisionAction::Cancel));
    }

    #[test]
    fn permission_response_unknown_decision_maps_to_cancel() {
        let d = map_permission_decision("fc_5", "something_else");
        assert!(matches!(d.resume.action, ResumeDecisionAction::Cancel));
    }

    #[test]
    fn unknown_method_returns_error_response() {
        let line = make_error_response(json!(99), -32601, "method not found: foo/bar");
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["error"]["code"], -32601);
        assert_eq!(parsed["error"]["message"], "method not found: foo/bar");
        assert_eq!(parsed["id"], 99);
    }

    #[test]
    fn parse_error_response_has_null_id() {
        let line = make_error_response(Value::Null, -32700, "parse error");
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert!(parsed["id"].is_null());
        assert_eq!(parsed["error"]["code"], -32700);
    }

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
}

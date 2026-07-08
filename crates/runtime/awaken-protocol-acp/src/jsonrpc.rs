//! The official-ACP JSON-RPC 2.0 client driver (`real-acp` feature).
//!
//! This is the production wire behind [`crate::Codec::Acp`]: it speaks the real
//! `agent-client-protocol` over an [`AgentChannel`], driving one prompt turn —
//! `initialize` → `session/new` → `session/prompt` → stream `session/update`
//! notifications until the prompt response carries a `stopReason`. The official
//! *schema types* are the wire vocabulary; the loop is hand-driven (not the
//! crate's `ClientSideConnection`, whose `!Send` `LocalBoxFuture` driver would
//! force a dedicated-thread `LocalSet` and break the `Send` `RunExecutor`).
//!
//! Projection and the [`RunEventSink`] contract are identical to the newline
//! stand-in ([`crate::AcpBridge`]) — a `session/update` becomes the same
//! [`AgentEvent`], so nothing downstream (the store, the executor) sees ACP
//! vocabulary. Agent→client requests are answered fail-closed: `session/
//! request_permission` selects a reject option (we advertise no `fs`/`terminal`
//! capabilities, so those requests get `method_not_found`).

use agent_client_protocol::{
    AGENT_METHOD_NAMES, CLIENT_METHOD_NAMES, ClientCapabilities, ContentBlock, InitializeRequest,
    NewSessionRequest, NewSessionResponse, PermissionOptionKind, PromptRequest, PromptResponse,
    ProtocolVersion, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification,
};
use awaken_agent_channel::AgentChannel;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::real_acp::{project_update, termination_from_stop_reason};
use crate::{
    AcpError, AcpLaunchEvent, AcpLaunchStage, AgentEvent, LaunchSink, RunEventSink,
    TerminationReason, notify_launch,
};

const JSONRPC: &str = "2.0";
const ID_INITIALIZE: u64 = 1;
const ID_NEW_SESSION: u64 = 2;
const ID_PROMPT: u64 = 3;
/// JSON-RPC "method not found" (the reply to any capability we do not advertise).
const METHOD_NOT_FOUND: i64 = -32601;

/// One outbound JSON-RPC request: `{"jsonrpc","id","method","params"}`.
#[derive(Serialize)]
struct OutRequest<'a, P: Serialize> {
    jsonrpc: &'a str,
    id: u64,
    method: &'a str,
    params: P,
}

/// One outbound JSON-RPC result response to an inbound request id.
#[derive(Serialize)]
struct OutResult<'a, R: Serialize> {
    jsonrpc: &'a str,
    id: serde_json::Value,
    result: R,
}

/// One outbound JSON-RPC error response to an inbound request id.
#[derive(Serialize)]
struct OutError<'a> {
    jsonrpc: &'a str,
    id: serde_json::Value,
    error: RpcErrorBody<'a>,
}

#[derive(Serialize)]
struct RpcErrorBody<'a> {
    code: i64,
    message: &'a str,
}

/// A parsed inbound line — response (`result`/`error` + `id`), agent→client
/// request (`id` + `method`), or notification (`method`, no `id`).
#[derive(serde::Deserialize)]
struct Incoming {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<serde_json::Value>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

/// The newline-delimited JSON-RPC transport over one agent channel: one message
/// per line (the wire the official stdio connection uses).
struct Wire<'a> {
    reader: BufReader<&'a mut dyn AgentChannel>,
    line: String,
}

impl<'a> Wire<'a> {
    fn new(channel: &'a mut dyn AgentChannel) -> Self {
        Self {
            reader: BufReader::new(channel),
            line: String::new(),
        }
    }

    async fn send<P: Serialize>(&mut self, msg: &P) -> Result<(), AcpError> {
        let mut buf = serde_json::to_vec(msg).map_err(|e| AcpError::Frame(e.to_string()))?;
        buf.push(b'\n');
        self.reader
            .get_mut()
            .write_all(&buf)
            .await
            .map_err(|e| AcpError::Io(e.to_string()))?;
        self.reader
            .get_mut()
            .flush()
            .await
            .map_err(|e| AcpError::Io(e.to_string()))
    }

    async fn send_request<P: Serialize>(
        &mut self,
        id: u64,
        method: &str,
        params: P,
    ) -> Result<(), AcpError> {
        self.send(&OutRequest {
            jsonrpc: JSONRPC,
            id,
            method,
            params,
        })
        .await
    }

    /// Read one message, or `None` at end of stream.
    async fn read(&mut self) -> Result<Option<Incoming>, AcpError> {
        loop {
            self.line.clear();
            let n = self
                .reader
                .read_line(&mut self.line)
                .await
                .map_err(|e| AcpError::Io(e.to_string()))?;
            if n == 0 {
                return Ok(None);
            }
            let trimmed = self.line.trim();
            if trimmed.is_empty() {
                continue;
            }
            return serde_json::from_str::<Incoming>(trimmed)
                .map(Some)
                .map_err(|e| AcpError::Frame(e.to_string()));
        }
    }
}

/// Drive one ACP prompt turn over `channel`, projecting each `session/update`
/// into `sink` with a strictly increasing seq, and returning the reason carried
/// by the prompt's `stopReason`.
pub async fn run_turn(
    channel: &mut dyn AgentChannel,
    prompt: &str,
    sink: &mut dyn RunEventSink,
    launch_sink: Option<LaunchSink<'_>>,
) -> Result<TerminationReason, AcpError> {
    let mut wire = Wire::new(channel);
    let mut seq = 0u64;

    // The process is up; the ACP handshake (initialize + session/new) begins now.
    notify_launch(
        launch_sink,
        AcpLaunchEvent::stage(AcpLaunchStage::Initializing),
    );

    // 1. initialize — advertise the latest protocol version and no fs/terminal
    //    capabilities (default `ClientCapabilities`), so those agent requests are
    //    refused fail-closed later.
    wire.send_request(
        ID_INITIALIZE,
        AGENT_METHOD_NAMES.initialize,
        // Default `ClientCapabilities` = no fs/terminal, so those agent requests are
        // refused fail-closed later.
        InitializeRequest::new(ProtocolVersion::LATEST)
            .client_capabilities(ClientCapabilities::default()),
    )
    .await?;
    pump_to_response(&mut wire, ID_INITIALIZE, sink, &mut seq).await?;

    // 2. session/new — a fresh session rooted at the sandbox cwd, no MCP servers.
    wire.send_request(
        ID_NEW_SESSION,
        AGENT_METHOD_NAMES.session_new,
        NewSessionRequest::new("/"),
    )
    .await?;
    let new_session: NewSessionResponse =
        parse(pump_to_response(&mut wire, ID_NEW_SESSION, sink, &mut seq).await?)?;

    // Handshake complete — the agent is live and about to accept the prompt.
    notify_launch(launch_sink, AcpLaunchEvent::stage(AcpLaunchStage::Ready));

    // 3. session/prompt — the user turn as one text content block.
    wire.send_request(
        ID_PROMPT,
        AGENT_METHOD_NAMES.session_prompt,
        PromptRequest::new(new_session.session_id, vec![ContentBlock::from(prompt)]),
    )
    .await?;
    let prompt_result = pump_to_response(&mut wire, ID_PROMPT, sink, &mut seq).await?;
    let response: PromptResponse = parse(prompt_result)?;
    let reason = termination_from_stop_reason(response.stop_reason);
    seq += 1;
    sink.append(seq, &AgentEvent::TurnEnd { reason }).await?;
    Ok(reason)
}

/// Read messages until the response to `target_id` arrives, meanwhile projecting
/// `session/update` notifications into `sink` and answering agent→client requests
/// fail-closed. Returns the matching response's `result`, or an error if the
/// agent answered `target_id` with a JSON-RPC error or the stream ended first.
async fn pump_to_response(
    wire: &mut Wire<'_>,
    target_id: u64,
    sink: &mut dyn RunEventSink,
    seq: &mut u64,
) -> Result<serde_json::Value, AcpError> {
    loop {
        let Some(msg) = wire.read().await? else {
            return Err(AcpError::Truncated);
        };
        match &msg.id {
            // A response or an agent→client request (both carry an id).
            Some(id) if msg.method.is_none() => {
                if id.as_u64() != Some(target_id) {
                    continue; // a stale response to an earlier id
                }
                if let Some(error) = msg.error {
                    return Err(AcpError::Frame(error.to_string()));
                }
                return Ok(msg.result.unwrap_or(serde_json::Value::Null));
            }
            Some(request_id) => {
                let method = msg.method.as_deref().unwrap_or_default();
                answer_request(wire, request_id.clone(), method, msg.params).await?;
            }
            // A notification (no id).
            None => {
                if msg.method.as_deref() == Some(CLIENT_METHOD_NAMES.session_update) {
                    project_notification(msg.params, sink, seq).await?;
                }
            }
        }
    }
}

/// Project one `session/update` notification into the sink (skips updates with no
/// runtime projection: user echoes, thoughts, plans, tool-call updates).
async fn project_notification(
    params: Option<serde_json::Value>,
    sink: &mut dyn RunEventSink,
    seq: &mut u64,
) -> Result<(), AcpError> {
    let Some(params) = params else {
        return Ok(());
    };
    let notification: SessionNotification = parse(params)?;
    if let Some(event) = project_update(&notification.update) {
        *seq += 1;
        sink.append(*seq, &event).await?;
    }
    Ok(())
}

/// Answer an agent→client request fail-closed: a permission request selects a
/// reject option (or `cancelled` if none is offered); every other method — the
/// `fs`/`terminal` capabilities we never advertised — gets `method_not_found`.
async fn answer_request(
    wire: &mut Wire<'_>,
    id: serde_json::Value,
    method: &str,
    params: Option<serde_json::Value>,
) -> Result<(), AcpError> {
    if method == CLIENT_METHOD_NAMES.session_request_permission {
        let outcome = params
            .and_then(|p| parse::<RequestPermissionRequest>(p).ok())
            .map(|req| reject_outcome(&req))
            .unwrap_or(RequestPermissionOutcome::Cancelled);
        return wire
            .send(&OutResult {
                jsonrpc: JSONRPC,
                id,
                result: RequestPermissionResponse::new(outcome),
            })
            .await;
    }
    wire.send(&OutError {
        jsonrpc: JSONRPC,
        id,
        error: RpcErrorBody {
            code: METHOD_NOT_FOUND,
            message: "capability not supported by this client",
        },
    })
    .await
}

/// The fail-closed permission decision: pick a reject option if the agent offered
/// one (once-preferred over always), else report the turn cancelled.
fn reject_outcome(req: &RequestPermissionRequest) -> RequestPermissionOutcome {
    let reject = req
        .options
        .iter()
        .find(|o| o.kind == PermissionOptionKind::RejectOnce)
        .or_else(|| {
            req.options
                .iter()
                .find(|o| o.kind == PermissionOptionKind::RejectAlways)
        });
    match reject {
        Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            option.option_id.clone(),
        )),
        None => RequestPermissionOutcome::Cancelled,
    }
}

fn parse<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, AcpError> {
    serde_json::from_value(value).map_err(|e| AcpError::Frame(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    use crate::SinkError;

    #[derive(Default)]
    struct RecordingSink {
        last: u64,
        events: Vec<(u64, AgentEvent)>,
    }

    #[async_trait]
    impl RunEventSink for RecordingSink {
        async fn append(&mut self, seq: u64, event: &AgentEvent) -> Result<(), SinkError> {
            if seq <= self.last {
                return Err(SinkError::NonMonotonic {
                    got: seq,
                    last: self.last,
                });
            }
            self.last = seq;
            self.events.push((seq, event.clone()));
            Ok(())
        }
    }

    /// A read/write half over one side of the duplex — reads request lines and
    /// writes reply lines through the same buffered stream (no split-borrow).
    struct AgentIo {
        reader: BufReader<DuplexStream>,
        line: String,
    }

    impl AgentIo {
        fn new(side: DuplexStream) -> Self {
            Self {
                reader: BufReader::new(side),
                line: String::new(),
            }
        }

        /// Read one JSON line, or `None` at EOF.
        async fn read(&mut self) -> Option<serde_json::Value> {
            self.line.clear();
            let n = self.reader.read_line(&mut self.line).await.unwrap();
            (n != 0).then(|| serde_json::from_str(self.line.trim()).unwrap())
        }

        async fn write_line(&mut self, line: &str) {
            let stream = self.reader.get_mut();
            stream.write_all(line.as_bytes()).await.unwrap();
            stream.write_all(b"\n").await.unwrap();
            stream.flush().await.unwrap();
        }
    }

    /// A scripted in-process ACP agent speaking real JSON-RPC over the duplex: it
    /// answers `initialize`/`session/new`, then — after `session/prompt` — emits
    /// `scripted` (raw JSON-RPC lines: notifications and/or a permission request),
    /// and finally replies to the prompt with the given stop reason.
    async fn scripted_agent(side: DuplexStream, scripted: Vec<String>, stop_reason: &str) {
        let mut io = AgentIo::new(side);
        // Answer requests by id in order: 1=initialize, 2=session/new, 3=prompt.
        while let Some(msg) = io.read().await {
            match msg.get("id").and_then(|v| v.as_u64()) {
                Some(1) => {
                    io.write_line(
                        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
                    )
                    .await;
                }
                Some(2) => {
                    io.write_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1"}}"#)
                        .await;
                }
                Some(3) => {
                    // The prompt arrived: emit the scripted mid-turn lines, then the
                    // prompt response with the stop reason.
                    for l in &scripted {
                        io.write_line(l).await;
                    }
                    io.write_line(&format!(
                        r#"{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"{stop_reason}"}}}}"#
                    ))
                    .await;
                    // Drain a possible permission reply so the client's write lands.
                    if scripted.iter().any(|l| l.contains("request_permission")) {
                        let _ = io.read().await;
                    }
                    return;
                }
                _ => return,
            }
        }
    }

    fn channel() -> (Box<dyn AgentChannel>, DuplexStream) {
        let (ours, theirs) = tokio::io::duplex(8192);
        (Box::new(ours), theirs)
    }

    #[tokio::test]
    async fn drives_handshake_prompt_and_projects_message_updates() {
        let (mut ours, theirs) = channel();
        let updates = vec![
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello from acp"}}}}"#.into(),
        ];
        let agent = tokio::spawn(scripted_agent(theirs, updates, "end_turn"));

        let mut sink = RecordingSink::default();
        let reason = run_turn(ours.as_mut(), "do it", &mut sink, None)
            .await
            .unwrap();
        assert_eq!(reason, TerminationReason::NaturalEnd);
        // One projected message + the synthetic TurnEnd, in seq order.
        assert!(matches!(
            &sink.events[0].1,
            AgentEvent::Message { text } if text == "hello from acp"
        ));
        assert!(matches!(
            sink.events.last().unwrap().1,
            AgentEvent::TurnEnd {
                reason: TerminationReason::NaturalEnd
            }
        ));
        assert_eq!(sink.events[0].0, 1);
        assert_eq!(sink.events.last().unwrap().0, 2);
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn refusal_stop_reason_projects_a_refusal() {
        let (mut ours, theirs) = channel();
        let agent = tokio::spawn(scripted_agent(theirs, vec![], "refusal"));
        let mut sink = RecordingSink::default();
        let reason = run_turn(ours.as_mut(), "p", &mut sink, None).await.unwrap();
        assert_eq!(reason, TerminationReason::Refusal);
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn tool_call_update_projects_input_from_raw_input() {
        let (mut ours, theirs) = channel();
        let updates = vec![
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"read","rawInput":{"path":"a.txt"}}}}"#.into(),
        ];
        let agent = tokio::spawn(scripted_agent(theirs, updates, "end_turn"));
        let mut sink = RecordingSink::default();
        run_turn(ours.as_mut(), "p", &mut sink, None).await.unwrap();
        let tool = sink
            .events
            .iter()
            .find_map(|(_, e)| match e {
                AgentEvent::ToolCall { name, input } => Some((name.clone(), input.clone())),
                _ => None,
            })
            .expect("a tool call was projected");
        assert_eq!(tool.0, "read");
        assert_eq!(tool.1["path"], "a.txt");
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn permission_request_is_answered_fail_closed_with_a_reject_option() {
        let (mut ours, theirs) = channel();
        let seen = Arc::new(Mutex::new(String::new()));
        // The agent asks permission mid-turn; we must select the reject option.
        let perm = r#"{"jsonrpc":"2.0","id":42,"method":"session/request_permission","params":{"sessionId":"sess-1","toolCall":{"toolCallId":"t1"},"options":[{"optionId":"ok","name":"Allow","kind":"allow_once"},{"optionId":"no","name":"Reject","kind":"reject_once"}]}}"#;
        let seen2 = seen.clone();
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            // 1 initialize, 2 new session
            io.read().await;
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            )
            .await;
            io.read().await;
            io.write_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1"}}"#)
                .await;
            // read the prompt request (id 3), then send the permission request
            io.read().await;
            io.write_line(perm).await;
            // read the client's permission reply
            io.read().await;
            *seen2.lock().unwrap() = io.line.clone();
            // finish the prompt
            io.write_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
                .await;
        });

        let mut sink = RecordingSink::default();
        let reason = run_turn(ours.as_mut(), "p", &mut sink, None).await.unwrap();
        assert_eq!(reason, TerminationReason::NaturalEnd);
        agent.await.unwrap();
        let reply = seen.lock().unwrap().clone();
        assert!(
            reply.contains("\"id\":42"),
            "replied to the request id: {reply}"
        );
        assert!(reply.contains("selected"), "selected an option: {reply}");
        assert!(reply.contains("\"no\""), "the reject option id: {reply}");
    }

    #[tokio::test]
    async fn truncated_before_prompt_response_errors() {
        let (mut ours, theirs) = channel();
        // Agent answers handshake then closes without a prompt response.
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            io.read().await;
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            )
            .await;
            io.read().await;
            io.write_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1"}}"#)
                .await;
            io.read().await; // prompt request, then drop without responding
        });
        let mut sink = RecordingSink::default();
        let err = run_turn(ours.as_mut(), "p", &mut sink, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AcpError::Truncated));
        agent.await.unwrap();
    }
}

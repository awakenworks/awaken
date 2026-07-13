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
//! Projection and the [`RunFactAppender`] contract are identical to the newline
//! stand-in ([`crate::AcpBridge`]) — a `session/update` becomes the same
//! [`AgentEvent`], so nothing downstream (the store, the executor) sees ACP
//! vocabulary. A `session/request_permission` is decided by the injected
//! [`crate::PermissionResolver`] (the neutral `PermissionPolicy` behind an executor
//! adapter) and projected back onto the agent's own allow/reject option. We
//! advertise no `fs`/`terminal` capabilities, so those agent requests still get
//! `method_not_found` — tool execution is the hand's job, never proxied over ACP.

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
    AcpError, AcpLaunchEvent, AcpLaunchStage, AgentEvent, AllowAll, LaunchSink, PermissionAsk,
    PermissionResolver, PermissionVerdict, RunFactAppender, TerminationReason, notify_launch,
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
    sink: &mut dyn RunFactAppender,
    launch_sink: Option<LaunchSink<'_>>,
) -> Result<TerminationReason, AcpError> {
    run_turn_with_permission(channel, prompt, sink, &AllowAll, launch_sink).await
}

/// [`run_turn`], authorizing the agent's mid-turn `session/request_permission`
/// requests through `resolver` (the neutral `PermissionPolicy` behind an executor
/// adapter). `fs`/`terminal` requests are still refused `method_not_found` — tool
/// execution is the hand's job, never proxied back over ACP.
pub async fn run_turn_with_permission(
    channel: &mut dyn AgentChannel,
    prompt: &str,
    sink: &mut dyn RunFactAppender,
    resolver: &dyn PermissionResolver,
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
    pump_to_response(&mut wire, ID_INITIALIZE, sink, &mut seq, resolver).await?;

    // 2. session/new — a fresh session rooted at the sandbox cwd, no MCP servers.
    wire.send_request(
        ID_NEW_SESSION,
        AGENT_METHOD_NAMES.session_new,
        NewSessionRequest::new("/"),
    )
    .await?;
    let new_session: NewSessionResponse =
        parse(pump_to_response(&mut wire, ID_NEW_SESSION, sink, &mut seq, resolver).await?)?;

    // Handshake complete — the agent is live and about to accept the prompt.
    notify_launch(launch_sink, AcpLaunchEvent::stage(AcpLaunchStage::Ready));

    // 3. session/prompt — the user turn as one text content block.
    wire.send_request(
        ID_PROMPT,
        AGENT_METHOD_NAMES.session_prompt,
        PromptRequest::new(new_session.session_id, vec![ContentBlock::from(prompt)]),
    )
    .await?;
    let prompt_result = pump_to_response(&mut wire, ID_PROMPT, sink, &mut seq, resolver).await?;
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
    sink: &mut dyn RunFactAppender,
    seq: &mut u64,
    resolver: &dyn PermissionResolver,
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
                answer_request(wire, request_id.clone(), method, msg.params, resolver).await?;
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
    sink: &mut dyn RunFactAppender,
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

/// Answer an agent→client request: a permission request is decided by `resolver`
/// (the neutral `PermissionPolicy`) and projected back onto the agent's own
/// offered option — allow or reject, once-preferred over always; `cancelled` when
/// no matching option is offered. Every other method — the `fs`/`terminal`
/// capabilities we never advertised — gets `method_not_found`, because tool
/// execution is the hand's job and is never proxied back over ACP.
async fn answer_request(
    wire: &mut Wire<'_>,
    id: serde_json::Value,
    method: &str,
    params: Option<serde_json::Value>,
    resolver: &dyn PermissionResolver,
) -> Result<(), AcpError> {
    if method == CLIENT_METHOD_NAMES.session_request_permission {
        let outcome = match params.and_then(|p| {
            parse::<RequestPermissionRequest>(p.clone())
                .ok()
                .map(|r| (p, r))
        }) {
            Some((raw, req)) => {
                let verdict = resolver.resolve(&permission_ask(&raw)).await;
                select_outcome(&req, verdict)
            }
            None => RequestPermissionOutcome::Cancelled,
        };
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

/// Project a raw `session/request_permission` params object into a neutral
/// [`PermissionAsk`] — the tool's title/kind, its `toolCallId`, and its `rawInput`
/// — read loosely so the ask survives adapter-to-adapter shape differences.
fn permission_ask(raw: &serde_json::Value) -> PermissionAsk {
    let tool_call = raw.get("toolCall");
    let tool = tool_call
        .and_then(|tc| tc.get("title").or_else(|| tc.get("kind")))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let call_id = tool_call
        .and_then(|tc| tc.get("toolCallId"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let arguments = tool_call
        .and_then(|tc| tc.get("rawInput"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    PermissionAsk {
        tool,
        call_id,
        arguments,
    }
}

/// Project a [`PermissionVerdict`] onto the agent's own offered option: an
/// `Allow` picks `allow_once` (else `allow_always`); a `Deny` picks `reject_once`
/// (else `reject_always`). When the agent offered no option of the decided kind
/// the turn is cancelled (a well-behaved agent always offers both).
fn select_outcome(
    req: &RequestPermissionRequest,
    verdict: PermissionVerdict,
) -> RequestPermissionOutcome {
    let (once, always) = match verdict {
        PermissionVerdict::Allow => (
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ),
        PermissionVerdict::Deny => (
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ),
    };
    let chosen = req
        .options
        .iter()
        .find(|o| o.kind == once)
        .or_else(|| req.options.iter().find(|o| o.kind == always));
    match chosen {
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

    use crate::AppendError;

    #[derive(Default)]
    struct RecordingSink {
        last: u64,
        events: Vec<(u64, AgentEvent)>,
    }

    #[async_trait]
    impl RunFactAppender for RecordingSink {
        async fn append(&mut self, seq: u64, event: &AgentEvent) -> Result<(), AppendError> {
            if seq <= self.last {
                return Err(AppendError::NonMonotonic {
                    got: seq,
                    last: self.last,
                });
            }
            self.last = seq;
            self.events.push((seq, event.clone()));
            Ok(())
        }
    }

    /// A resolver that denies every ask — stands in for a `PermissionPolicy` that
    /// refuses the CLI's tool.
    struct DenyAll;
    #[async_trait]
    impl PermissionResolver for DenyAll {
        async fn resolve(&self, _ask: &PermissionAsk) -> PermissionVerdict {
            PermissionVerdict::Deny
        }
    }

    /// A resolver that records the ask it saw, then allows — proves the driver
    /// projects the agent's request into a neutral [`PermissionAsk`].
    #[derive(Default)]
    struct RecordingResolver {
        seen: Mutex<Vec<PermissionAsk>>,
    }
    #[async_trait]
    impl PermissionResolver for RecordingResolver {
        async fn resolve(&self, ask: &PermissionAsk) -> PermissionVerdict {
            self.seen.lock().unwrap().push(ask.clone());
            PermissionVerdict::Allow
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
    async fn a_non_projecting_update_is_skipped_while_the_message_still_projects() {
        let (mut ours, theirs) = channel();
        let updates = vec![
            // A user-message echo has no runtime projection — it must be skipped.
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"echo of the prompt"}}}}"#.into(),
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"the answer"}}}}"#.into(),
        ];
        let agent = tokio::spawn(scripted_agent(theirs, updates, "end_turn"));
        let mut sink = RecordingSink::default();
        run_turn(ours.as_mut(), "p", &mut sink, None).await.unwrap();
        // Only the agent message projected; the user echo produced no event.
        let messages: Vec<String> = sink
            .events
            .iter()
            .filter_map(|(_, e)| match e {
                AgentEvent::Message { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(messages, vec!["the answer".to_string()]);
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
    async fn cancelled_stop_reason_maps_to_cancelled_over_the_live_driver() {
        let (mut ours, theirs) = channel();
        let agent = tokio::spawn(scripted_agent(theirs, vec![], "cancelled"));
        let mut sink = RecordingSink::default();
        let reason = run_turn(ours.as_mut(), "p", &mut sink, None).await.unwrap();
        assert_eq!(reason, TerminationReason::Cancelled);
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn max_tokens_stop_reason_maps_to_timed_out_over_the_live_driver() {
        let (mut ours, theirs) = channel();
        let agent = tokio::spawn(scripted_agent(theirs, vec![], "max_tokens"));
        let mut sink = RecordingSink::default();
        let reason = run_turn(ours.as_mut(), "p", &mut sink, None).await.unwrap();
        assert_eq!(reason, TerminationReason::TimedOut);
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
                AgentEvent::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
                _ => None,
            })
            .expect("a tool call was projected");
        assert_eq!(tool.0, "read");
        assert_eq!(tool.1["path"], "a.txt");
        agent.await.unwrap();
    }

    /// A permission request offering both an allow and a reject option, over a tool
    /// with a title and raw input (so the projected ask carries them).
    const PERM: &str = r#"{"jsonrpc":"2.0","id":42,"method":"session/request_permission","params":{"sessionId":"sess-1","toolCall":{"toolCallId":"t1","title":"bash","rawInput":{"cmd":"ls"}},"options":[{"optionId":"ok","name":"Allow","kind":"allow_once"},{"optionId":"no","name":"Reject","kind":"reject_once"}]}}"#;

    /// Drive one turn where the agent asks `PERM` mid-turn, decided by `resolver`;
    /// return the client's raw permission reply line.
    async fn drive_permission(resolver: &dyn PermissionResolver) -> String {
        let (mut ours, theirs) = channel();
        let seen = Arc::new(Mutex::new(String::new()));
        let seen2 = seen.clone();
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
            io.read().await; // prompt request (id 3)
            io.write_line(PERM).await;
            io.read().await; // the client's permission reply
            *seen2.lock().unwrap() = io.line.clone();
            io.write_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
                .await;
        });

        let mut sink = RecordingSink::default();
        let reason = run_turn_with_permission(ours.as_mut(), "p", &mut sink, resolver, None)
            .await
            .unwrap();
        assert_eq!(reason, TerminationReason::NaturalEnd);
        agent.await.unwrap();
        let reply = seen.lock().unwrap().clone();
        reply
    }

    #[tokio::test]
    async fn a_denying_policy_selects_the_agents_reject_option() {
        // The neutral policy denies → we select the agent's own reject option.
        let reply = drive_permission(&DenyAll).await;
        assert!(
            reply.contains("\"id\":42"),
            "replied to the request id: {reply}"
        );
        assert!(reply.contains("selected"), "selected an option: {reply}");
        assert!(reply.contains("\"no\""), "the reject option id: {reply}");
    }

    #[tokio::test]
    async fn an_allowing_policy_selects_the_agents_allow_option_and_projects_the_ask() {
        // The neutral policy allows → we select the agent's own allow option, and
        // the driver projected the request into a neutral ask (tool + args + id).
        let resolver = RecordingResolver::default();
        let reply = drive_permission(&resolver).await;
        assert!(reply.contains("selected"), "selected an option: {reply}");
        assert!(reply.contains("\"ok\""), "the allow option id: {reply}");
        let seen = resolver.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "the resolver saw exactly one ask");
        assert_eq!(seen[0].tool, "bash");
        assert_eq!(seen[0].call_id, "t1");
        assert_eq!(seen[0].arguments["cmd"], "ls");
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

    #[tokio::test]
    async fn fs_request_gets_method_not_found_because_we_advertise_no_fs_capability() {
        // We advertise no `fs`/`terminal` capabilities; an agent that asks for one
        // mid-turn must be answered fail-closed with a JSON-RPC method_not_found, not
        // silently granted. This is the core "we serve no client capabilities" claim.
        let (mut ours, theirs) = channel();
        let seen = Arc::new(Mutex::new(String::new()));
        let fs_req = r#"{"jsonrpc":"2.0","id":42,"method":"fs/read_text_file","params":{"sessionId":"sess-1","path":"/etc/passwd"}}"#;
        let seen2 = seen.clone();
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
            io.read().await; // prompt request
            io.write_line(fs_req).await;
            io.read().await; // the client's fail-closed reply
            *seen2.lock().unwrap() = io.line.clone();
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
        assert!(reply.contains("-32601"), "method_not_found code: {reply}");
        assert!(
            reply.contains("capability not supported"),
            "names the unsupported capability: {reply}"
        );
    }

    fn perm_req(options: serde_json::Value) -> RequestPermissionRequest {
        serde_json::from_value(serde_json::json!({
            "sessionId": "sess-1",
            "toolCall": { "toolCallId": "t1" },
            "options": options,
        }))
        .expect("a valid permission request")
    }

    fn outcome_json(outcome: &RequestPermissionOutcome) -> String {
        serde_json::to_value(outcome).unwrap().to_string()
    }

    #[test]
    fn reject_once_is_preferred_over_allow_and_reject_always() {
        let req = perm_req(serde_json::json!([
            {"optionId":"ok","name":"Allow","kind":"allow_once"},
            {"optionId":"no","name":"Reject","kind":"reject_once"},
            {"optionId":"never","name":"Reject always","kind":"reject_always"},
        ]));
        let out = select_outcome(&req, PermissionVerdict::Deny);
        assert!(matches!(out, RequestPermissionOutcome::Selected(_)));
        assert!(
            outcome_json(&out).contains("\"no\""),
            "{}",
            outcome_json(&out)
        );
    }

    #[test]
    fn reject_always_is_the_fallback_when_no_reject_once() {
        let req = perm_req(serde_json::json!([
            {"optionId":"ok","name":"Allow","kind":"allow_once"},
            {"optionId":"never","name":"Reject always","kind":"reject_always"},
        ]));
        let out = select_outcome(&req, PermissionVerdict::Deny);
        assert!(matches!(out, RequestPermissionOutcome::Selected(_)));
        assert!(outcome_json(&out).contains("\"never\""));
    }

    #[test]
    fn no_reject_option_offered_cancels_the_turn() {
        let req = perm_req(serde_json::json!([
            {"optionId":"ok","name":"Allow","kind":"allow_once"},
        ]));
        assert!(matches!(
            select_outcome(&req, PermissionVerdict::Deny),
            RequestPermissionOutcome::Cancelled
        ));
    }
}

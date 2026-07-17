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
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PermissionOptionKind, PromptRequest, PromptResponse, ProtocolVersion,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionId, SessionModeId, SessionModeState, SessionNotification,
    SetSessionModeRequest,
};
use awaken_agent_channel::AgentChannel;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Map the neutral [`crate::SessionMcpServer`]s onto the ACP `session/new` `mcpServers`
/// param: an HTTP server carries its auth as a header, a stdio server as an env var
/// (α: a broker reference the gateway resolves; β: a raw secret on a trusted launch).
fn to_acp_mcp_servers(
    servers: &[crate::SessionMcpServer],
) -> Vec<agent_client_protocol::McpServer> {
    use agent_client_protocol::{
        EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerStdio,
    };
    servers
        .iter()
        .map(|s| match &s.url {
            Some(url) => {
                let headers = s
                    .auth
                    .iter()
                    .map(|(n, v)| HttpHeader::new(n.clone(), v.clone()))
                    .collect();
                McpServer::Http(McpServerHttp::new(s.name.clone(), url.clone()).headers(headers))
            }
            None => {
                let env = s
                    .auth
                    .iter()
                    .map(|(n, v)| EnvVariable::new(n.clone(), v.clone()))
                    .collect();
                McpServer::Stdio(
                    McpServerStdio::new(s.name.clone(), s.command.clone().unwrap_or_default())
                        .args(s.args.clone())
                        .env(env),
                )
            }
        })
        .collect()
}

use crate::real_acp::{project_update, termination_from_stop_reason};
use crate::{
    AcpError, AcpLaunchEvent, AcpLaunchStage, AgentEvent, AllowAll, LaunchSink, PermissionAsk,
    PermissionResolver, PermissionVerdict, RunFactAppender, TerminationReason, TurnConfig,
    notify_launch,
};

const JSONRPC: &str = "2.0";
const ID_INITIALIZE: u64 = 1;
const ID_NEW_SESSION: u64 = 2;
const ID_PROMPT: u64 = 3;
/// `session/set_mode` (only sent when a mode is pinned) — a distinct correlation
/// id; JSON-RPC ids need only be unique, not ordered.
const ID_SET_MODE: u64 = 4;
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
    let mut config = TurnConfig::new(&AllowAll);
    run_turn_with_config(channel, prompt, sink, &mut config, launch_sink).await
}

/// [`run_turn`] driven by a [`TurnConfig`]: authorize the agent's mid-turn
/// `session/request_permission` through `config.resolver`, and — when
/// `config.session_id` is set and the agent advertises `loadSession` — resume that
/// session via `session/load` instead of `session/new` (so context survives the
/// per-turn relaunch); otherwise open a fresh session and record its id back into
/// `config.session_id` for the next turn. `fs`/`terminal` requests are still
/// refused `method_not_found` — tool execution is the hand's job.
pub async fn run_turn_with_config(
    channel: &mut dyn AgentChannel,
    prompt: &str,
    sink: &mut dyn RunFactAppender,
    config: &mut TurnConfig<'_>,
    launch_sink: Option<LaunchSink<'_>>,
) -> Result<TerminationReason, AcpError> {
    let resolver = config.resolver;
    // The interior cwd the session runs under — stable per thread so a cwd-keyed CLI
    // finds its session on `session/load` across directories (default `/`).
    let cwd = config
        .session_cwd
        .clone()
        .unwrap_or_else(|| "/".to_string());
    let mut wire = Wire::new(channel);
    let mut seq = 0u64;

    // The process is up; the ACP handshake (initialize + session/new|load) begins.
    notify_launch(
        launch_sink,
        AcpLaunchEvent::stage(AcpLaunchStage::Initializing),
    );

    // 1. initialize — advertise the latest protocol version and no fs/terminal
    //    capabilities (default `ClientCapabilities`), so those agent requests are
    //    refused fail-closed later. Read back the agent's capabilities to gate a
    //    session resume fail-closed (only load when the agent advertises it).
    wire.send_request(
        ID_INITIALIZE,
        AGENT_METHOD_NAMES.initialize,
        InitializeRequest::new(ProtocolVersion::LATEST)
            .client_capabilities(ClientCapabilities::default()),
    )
    .await?;
    let init: InitializeResponse =
        parse(pump_to_response(&mut wire, ID_INITIALIZE, sink, &mut seq, resolver).await?)?;
    let can_load = init.agent_capabilities.load_session;

    // 2. session/load (resume the CLI's own session across the relaunch) when we
    //    hold a prior id and the agent supports it; else session/new. Fail-safe:
    //    an agent that does not advertise `loadSession` falls back to a fresh
    //    session (the neutral thread history is the authority — never lost).
    let (session_id, available_modes): (SessionId, Vec<String>) = match config.session_id.clone() {
        Some(prior) if can_load => {
            wire.send_request(
                ID_NEW_SESSION,
                AGENT_METHOD_NAMES.session_load,
                LoadSessionRequest::new(SessionId::new(prior.as_str()), cwd.as_str()),
            )
            .await?;
            let resp: LoadSessionResponse = parse(
                pump_to_response(&mut wire, ID_NEW_SESSION, sink, &mut seq, resolver).await?,
            )?;
            (SessionId::new(prior.as_str()), mode_ids(resp.modes))
        }
        _ => {
            wire.send_request(
                ID_NEW_SESSION,
                AGENT_METHOD_NAMES.session_new,
                NewSessionRequest::new(cwd.as_str())
                    .mcp_servers(to_acp_mcp_servers(&config.mcp_servers)),
            )
            .await?;
            let new_session: NewSessionResponse = parse(
                pump_to_response(&mut wire, ID_NEW_SESSION, sink, &mut seq, resolver).await?,
            )?;
            let modes = mode_ids(new_session.modes);
            (new_session.session_id, modes)
        }
    };
    // Record the negotiated id so the caller resumes this session next turn.
    config.session_id = Some(session_id.to_string());

    // 2b. session/set_mode — pin the adapter's mode, fail-closed against the modes
    //     the agent advertised for the session (an unsupported pin never silently
    //     no-ops; it ends the turn with a classified fault).
    if let Some(mode) = config.session_mode.clone() {
        if !available_modes.contains(&mode) {
            return Err(AcpError::UnsupportedSessionMode(mode));
        }
        wire.send_request(
            ID_SET_MODE,
            AGENT_METHOD_NAMES.session_set_mode,
            SetSessionModeRequest::new(session_id.clone(), SessionModeId::new(mode.as_str())),
        )
        .await?;
        pump_to_response(&mut wire, ID_SET_MODE, sink, &mut seq, resolver).await?;
    }

    // Handshake complete — the agent is live and about to accept the prompt.
    notify_launch(launch_sink, AcpLaunchEvent::stage(AcpLaunchStage::Ready));

    // 3. session/prompt — the user turn as one text content block.
    wire.send_request(
        ID_PROMPT,
        AGENT_METHOD_NAMES.session_prompt,
        PromptRequest::new(session_id, vec![ContentBlock::from(prompt)]),
    )
    .await?;
    let prompt_result = pump_to_response(&mut wire, ID_PROMPT, sink, &mut seq, resolver).await?;
    let response: PromptResponse = parse(prompt_result)?;

    // Project the turn's token usage (when reported) so the executor records it as
    // committed thread usage, before the terminal `TurnEnd`. `real-acp` enables the
    // SDK's `unstable_session_usage`, so this field is always present here.
    if let Some(usage) = response.usage {
        seq += 1;
        sink.append(
            seq,
            &AgentEvent::Usage {
                prompt_tokens: usage.input_tokens,
                completion_tokens: usage.output_tokens,
                cache_read_tokens: usage.cached_read_tokens.unwrap_or(0),
                cache_creation_tokens: usage.cached_write_tokens.unwrap_or(0),
            },
        )
        .await?;
    }

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

/// The ids of the modes an agent advertised for a session (empty when it declares
/// none), used to validate a mode pin fail-closed.
fn mode_ids(modes: Option<SessionModeState>) -> Vec<String> {
    modes
        .map(|state| {
            state
                .available_modes
                .into_iter()
                .map(|mode| mode.id.0.to_string())
                .collect()
        })
        .unwrap_or_default()
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
        let mut config = TurnConfig::new(resolver);
        let reason = run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .unwrap();
        assert_eq!(reason, TerminationReason::NaturalEnd);
        agent.await.unwrap();

        seen.lock().unwrap().clone()
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
    async fn a_prior_session_is_resumed_via_session_load_when_the_agent_supports_it() {
        // Holding a session id and facing an agent that advertises loadSession, the
        // driver resumes via session/load (not new) and reports the same id back.
        let (mut ours, theirs) = channel();
        let saw_load = Arc::new(Mutex::new(false));
        let saw_load2 = saw_load.clone();
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            io.read().await; // initialize
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}"#,
            )
            .await;
            let req2 = io.read().await.unwrap(); // session/load
            *saw_load2.lock().unwrap() = req2.get("method").and_then(|m| m.as_str())
                == Some(AGENT_METHOD_NAMES.session_load);
            io.write_line(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#)
                .await;
            io.read().await; // prompt
            io.write_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
                .await;
        });

        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.session_id = Some("sess-resume".into());
        let reason = run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .unwrap();
        assert_eq!(reason, TerminationReason::NaturalEnd);
        assert!(*saw_load.lock().unwrap(), "the driver sent session/load");
        assert_eq!(
            config.session_id.as_deref(),
            Some("sess-resume"),
            "the resumed id is reported back for the next turn"
        );
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn the_interior_cwd_is_sent_on_session_new_and_load() {
        // The configured interior cwd (the sandbox's stable workspace path) is the
        // `cwd` on session/new and session/load — so a cwd-keyed CLI finds its
        // session across directories.
        let (mut ours, theirs) = channel();
        let cwds = Arc::new(Mutex::new(Vec::<String>::new()));
        let c2 = cwds.clone();
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            io.read().await; // initialize
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}"#,
            )
            .await;
            let sess = io.read().await.unwrap(); // session/load (we pass a prior id)
            if let Some(cwd) = sess
                .get("params")
                .and_then(|p| p.get("cwd"))
                .and_then(|v| v.as_str())
            {
                c2.lock().unwrap().push(cwd.to_string());
            }
            io.write_line(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#)
                .await;
            io.read().await; // prompt
            io.write_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
                .await;
        });

        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.session_id = Some("s1".into());
        config.session_cwd = Some("/workspace".into());
        run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .unwrap();
        assert_eq!(cwds.lock().unwrap().as_slice(), &["/workspace".to_string()]);
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn configured_mcp_servers_are_projected_onto_session_new() {
        // `to_acp_mcp_servers` is never exercised elsewhere (every other test leaves
        // `mcp_servers` empty). An HTTP server carries its url + auth as a header; a
        // stdio server carries command/args + auth as an env var. Capture the
        // session/new params and assert both shapes reach the wire.
        let (mut ours, theirs) = channel();
        let params = Arc::new(Mutex::new(serde_json::Value::Null));
        let p2 = params.clone();
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            io.read().await; // initialize
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            )
            .await;
            let new = io.read().await.unwrap(); // session/new
            *p2.lock().unwrap() = new
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            io.write_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#)
                .await;
            io.read().await; // prompt
            io.write_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
                .await;
        });

        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.mcp_servers = vec![
            crate::SessionMcpServer {
                name: "search".into(),
                command: None,
                args: Vec::new(),
                url: Some("https://mcp.example/sse".into()),
                auth: Some(("Authorization".into(), "Bearer tok".into())),
            },
            crate::SessionMcpServer {
                name: "fs".into(),
                command: Some("mcp-fs".into()),
                args: vec!["--root".into(), "/w".into()],
                url: None,
                auth: Some(("API_KEY".into(), "k1".into())),
            },
        ];
        run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .unwrap();

        {
            let params = params.lock().unwrap();
            let servers = params
                .get("mcpServers")
                .and_then(|v| v.as_array())
                .expect("mcpServers array present");
            assert_eq!(servers.len(), 2, "{params}");
            let text = params.to_string();
            // HTTP server: its url and the auth carried as a header.
            assert!(text.contains("https://mcp.example/sse"), "{text}");
            assert!(
                text.contains("Authorization") && text.contains("Bearer tok"),
                "http auth is a header: {text}"
            );
            // Stdio server: command + args, and the auth carried as an env var.
            assert!(text.contains("mcp-fs"), "{text}");
            assert!(text.contains("--root") && text.contains("/w"), "{text}");
            assert!(
                text.contains("API_KEY") && text.contains("k1"),
                "stdio auth is an env var: {text}"
            );
        }
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn a_prior_session_falls_back_to_new_when_the_agent_lacks_load() {
        // The agent does not advertise loadSession, so even with a prior id we open
        // a fresh session (fail-safe — the neutral thread history is the authority).
        let (mut ours, theirs) = channel();
        let method2 = Arc::new(Mutex::new(String::new()));
        let m2 = method2.clone();
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            io.read().await;
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            )
            .await;
            let req2 = io.read().await.unwrap();
            *m2.lock().unwrap() = req2
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            io.write_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"fresh"}}"#)
                .await;
            io.read().await;
            io.write_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
                .await;
        });

        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.session_id = Some("stale".into());
        run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .unwrap();
        assert_eq!(*method2.lock().unwrap(), AGENT_METHOD_NAMES.session_new);
        assert_eq!(
            config.session_id.as_deref(),
            Some("fresh"),
            "the fresh id replaced the stale one"
        );
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn a_pinned_session_mode_is_set_when_the_agent_advertises_it() {
        // With a mode pinned and the agent advertising it, the driver sends
        // session/set_mode with that mode id before prompting.
        let (mut ours, theirs) = channel();
        let saw = Arc::new(Mutex::new(None));
        let saw2 = saw.clone();
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            io.read().await; // initialize
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            )
            .await;
            io.read().await; // session/new
            io.write_line(
                r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1","modes":{"currentModeId":"default","availableModes":[{"id":"default","name":"Default"},{"id":"plan","name":"Plan"}]}}}"#,
            )
            .await;
            let set = io.read().await.unwrap(); // session/set_mode (id 4)
            if set.get("method").and_then(|m| m.as_str())
                == Some(AGENT_METHOD_NAMES.session_set_mode)
            {
                *saw2.lock().unwrap() = set
                    .get("params")
                    .and_then(|p| p.get("modeId"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
            io.write_line(r#"{"jsonrpc":"2.0","id":4,"result":{}}"#)
                .await;
            io.read().await; // prompt (id 3)
            io.write_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
                .await;
        });

        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.session_mode = Some("plan".into());
        let reason = run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .unwrap();
        assert_eq!(reason, TerminationReason::NaturalEnd);
        assert_eq!(saw.lock().unwrap().as_deref(), Some("plan"));
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn an_unadvertised_session_mode_fails_closed() {
        // The pinned mode is not among those the agent advertised → the turn ends
        // with a fail-closed error, not a silently ignored pin.
        let (mut ours, theirs) = channel();
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            io.read().await;
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            )
            .await;
            io.read().await; // session/new
            io.write_line(
                r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1","modes":{"currentModeId":"default","availableModes":[{"id":"default","name":"Default"}]}}}"#,
            )
            .await;
        });

        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.session_mode = Some("plan".into());
        let err = run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AcpError::UnsupportedSessionMode(m) if m == "plan"));
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn the_turn_usage_is_projected_as_a_usage_event() {
        // A prompt response carrying `usage` (unstable_session_usage) projects a
        // neutral Usage event (input→prompt, output→completion, caches mapped).
        let (mut ours, theirs) = channel();
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            io.read().await;
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            )
            .await;
            io.read().await;
            io.write_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#)
                .await;
            io.read().await; // prompt
            io.write_line(
                r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn","usage":{"totalTokens":30,"inputTokens":10,"outputTokens":20,"cachedReadTokens":5,"cachedWriteTokens":2}}}"#,
            )
            .await;
        });

        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .unwrap();
        let usage = sink
            .events
            .iter()
            .find_map(|(_, e)| match e {
                AgentEvent::Usage {
                    prompt_tokens,
                    completion_tokens,
                    cache_read_tokens,
                    cache_creation_tokens,
                } => Some((
                    *prompt_tokens,
                    *completion_tokens,
                    *cache_read_tokens,
                    *cache_creation_tokens,
                )),
                _ => None,
            })
            .expect("a usage event was projected");
        assert_eq!(usage, (10, 20, 5, 2));
        agent.await.unwrap();
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

    // ── Multi-chunk streaming + interleave ordering ──────────────────────────

    #[tokio::test]
    async fn multiple_agent_message_chunks_interleave_with_a_tool_call_in_seq_order() {
        // A real turn streams several `agent_message_chunk`s, possibly interleaved with
        // a tool call. The driver must project each into its own neutral event, in the
        // arrival order, at strictly increasing seqs (concatenation is the store's job
        // downstream — the ACL never merges chunks). Assert the exact ordered projection
        // Message("Hello ") → ToolCall(read) → Message("world") → TurnEnd.
        let (mut ours, theirs) = channel();
        let updates = vec![
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello "}}}}"#.into(),
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"tool_call","toolCallId":"c1","title":"read","rawInput":{"path":"a.txt"}}}}"#.into(),
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"world"}}}}"#.into(),
        ];
        let agent = tokio::spawn(scripted_agent(theirs, updates, "end_turn"));

        let mut sink = RecordingSink::default();
        let reason = run_turn(ours.as_mut(), "go", &mut sink, None)
            .await
            .unwrap();
        assert_eq!(reason, TerminationReason::NaturalEnd);

        // Exact ordered projection, including the synthetic terminal TurnEnd.
        let shape: Vec<AgentEvent> = sink.events.iter().map(|(_, e)| e.clone()).collect();
        assert_eq!(
            shape,
            vec![
                AgentEvent::Message {
                    text: "Hello ".into()
                },
                AgentEvent::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": "a.txt" }),
                },
                AgentEvent::Message {
                    text: "world".into()
                },
                AgentEvent::TurnEnd {
                    reason: TerminationReason::NaturalEnd,
                },
            ],
            "{shape:?}"
        );
        // Strictly increasing seqs, 1..=4, no gaps.
        let seqs: Vec<u64> = sink.events.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4]);
        agent.await.unwrap();
    }

    // ── JSON-RPC error-response + stale-id pump branches ─────────────────────

    #[tokio::test]
    async fn a_jsonrpc_error_response_to_the_prompt_surfaces_as_frame() {
        // `pump_to_response` returns the target id's `result`, but when the agent answers
        // that id with a JSON-RPC `error` object instead, the turn fails with
        // `AcpError::Frame` (the error-response arm — never reached by the happy-path
        // tests, which always answer with `result`).
        let (mut ours, theirs) = channel();
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            io.read().await; // initialize
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            )
            .await;
            io.read().await; // session/new
            io.write_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1"}}"#)
                .await;
            io.read().await; // prompt (id 3) — answered with an error, not a result
            io.write_line(
                r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"internal agent fault"}}"#,
            )
            .await;
        });

        let mut sink = RecordingSink::default();
        let err = run_turn(ours.as_mut(), "p", &mut sink, None)
            .await
            .unwrap_err();
        match err {
            AcpError::Frame(detail) => assert!(
                detail.contains("internal agent fault"),
                "the frame carries the agent's error body: {detail}"
            ),
            other => panic!("expected AcpError::Frame, got {other:?}"),
        }
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn a_stale_response_to_an_earlier_id_is_skipped_while_pumping() {
        // While pumping for the prompt response (id 3), a response bearing an unrelated
        // id (a late reply to an earlier request) must be skipped — the `continue`
        // branch — and the real id-3 response still resolves the turn. The projected
        // agent message proves the pump kept reading past the stale frame.
        let (mut ours, theirs) = channel();
        let updates = vec![
            // A stale response to id 99 arrives before the real prompt response.
            r#"{"jsonrpc":"2.0","id":99,"result":{"ignored":true}}"#.into(),
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"past the stale id"}}}}"#.into(),
        ];
        let agent = tokio::spawn(scripted_agent(theirs, updates, "end_turn"));

        let mut sink = RecordingSink::default();
        let reason = run_turn(ours.as_mut(), "p", &mut sink, None).await.unwrap();
        assert_eq!(reason, TerminationReason::NaturalEnd);
        assert!(
            sink.events.iter().any(|(_, e)| matches!(
                e,
                AgentEvent::Message { text } if text == "past the stale id"
            )),
            "the message after the stale id still projected: {:?}",
            sink.events
        );
        agent.await.unwrap();
    }

    // ── CHARACTERIZATION: `streamed_hard_limit` is UNWIRED here ───────────────

    #[tokio::test]
    async fn a_quota_banner_streamed_as_assistant_text_is_not_failed_closed() {
        // KNOWN BUG (adjudicate): `error::streamed_hard_limit` detects a provider's HARD
        // quota banner that arrives as assistant TEXT ("You've hit your weekly limit ·
        // resets …") — the case where the CLI then hangs — and classifies it RateLimited
        // so the turn can fail closed. But `run_turn` never consults it: an
        // `agent_message_chunk` is projected verbatim as `AgentEvent::Message` and the
        // turn ends on its `stopReason` like any other. This test PINS that current
        // (unwired) behavior: the banner is committed as a plain assistant message and
        // the turn ends `NaturalEnd`, NOT an Error/rate-limit termination.
        let banner = "You've hit your weekly limit · resets Jun 30";
        // Sanity: the detector itself WOULD flag this text — so the gap is purely that
        // the driver does not call it, not that the text is unrecognized.
        assert!(
            crate::streamed_hard_limit(banner).is_some(),
            "precondition: the banner is a recognized hard-limit banner"
        );

        let (mut ours, theirs) = channel();
        let updates = vec![format!(
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"sess-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{banner}"}}}}}}}}"#
        )];
        let agent = tokio::spawn(scripted_agent(theirs, updates, "end_turn"));

        let mut sink = RecordingSink::default();
        let reason = run_turn(ours.as_mut(), "p", &mut sink, None).await.unwrap();
        // The turn is NOT failed closed: it ends naturally…
        assert_eq!(
            reason,
            TerminationReason::NaturalEnd,
            "the quota banner does not fail the turn closed (unwired detector)"
        );
        // …and the banner is committed verbatim as an ordinary assistant message.
        assert!(
            sink.events.iter().any(|(_, e)| matches!(
                e,
                AgentEvent::Message { text } if text == banner
            )),
            "the banner is projected as plain assistant text: {:?}",
            sink.events
        );
        agent.await.unwrap();
    }
}

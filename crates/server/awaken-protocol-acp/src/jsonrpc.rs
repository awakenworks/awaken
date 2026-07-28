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
//! [`AcpProjectedEvent`], so nothing downstream (the store, the executor) sees ACP
//! vocabulary. A `session/request_permission` is decided by the injected
//! [`crate::PermissionResolver`] (the neutral `ToolPermissionPolicy` behind an executor
//! adapter) and projected back onto the agent's own allow/reject option. We
//! advertise no `fs`/`terminal` capabilities, so those agent requests still get
//! `method_not_found` — tool execution is the hand's job, never proxied over ACP.

use agent_client_protocol::{
    AGENT_METHOD_NAMES, AuthenticateRequest, AuthenticateResponse, CLIENT_METHOD_NAMES,
    ClientCapabilities, ContentBlock, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PermissionOptionKind,
    PromptRequest, PromptResponse, ProtocolVersion, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome, SessionId,
    SessionModeId, SessionNotification, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest,
};
use awaken_acp_contract::{AcpCapabilityProbeConfig, NegotiatedAcpCapabilities};
use awaken_agent_channel::AgentChannel;
use serde::Serialize;

mod wire;
use wire::{JSONRPC, Wire};

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
    AcpError, AcpLaunchEvent, AcpLaunchStage, AcpProjectedEvent, AllowAll, AppendError, LaunchSink,
    PermissionAsk, PermissionResolver, PermissionVerdict, RunFactAppender, TerminationReason,
    TurnConfig, notify_launch,
};

const ID_INITIALIZE: u64 = 1;
const ID_NEW_SESSION: u64 = 2;
const ID_PROMPT: u64 = 3;
/// `session/set_mode` (only sent when a mode is pinned) — a distinct correlation
/// id; JSON-RPC ids need only be unique, not ordered.
const ID_SET_MODE: u64 = 4;
/// Adapter-selected authentication, after initialize and before opening a Session.
const ID_AUTHENTICATE: u64 = 5;
/// Exact backend-owned model selection, after opening the Session and before
/// prompting. Managed/default launches never send this request.
const ID_SET_CONFIG_OPTION: u64 = 6;
/// JSON-RPC "method not found" (the reply to any capability we do not advertise).
const METHOD_NOT_FOUND: i64 = -32601;

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

async fn initialize_agent(
    wire: &mut Wire<'_>,
    sink: &mut dyn RunFactAppender,
    seq: &mut u64,
    resolver: &dyn PermissionResolver,
    auth_method_id: Option<&str>,
) -> Result<InitializeResponse, AcpError> {
    wire.send_request(
        ID_INITIALIZE,
        AGENT_METHOD_NAMES.initialize,
        InitializeRequest::new(ProtocolVersion::LATEST)
            .client_capabilities(ClientCapabilities::default()),
    )
    .await?;
    let init: InitializeResponse =
        parse(pump_to_response(wire, ID_INITIALIZE, sink, seq, resolver).await?)?;
    if let Some(method_id) = auth_method_id {
        if !init
            .auth_methods
            .iter()
            .any(|method| method.id().0.as_ref() == method_id)
        {
            return Err(AcpError::Frame(format!(
                "configured ACP authentication method `{method_id}` was not advertised"
            )));
        }
        wire.send_request(
            ID_AUTHENTICATE,
            AGENT_METHOD_NAMES.authenticate,
            AuthenticateRequest::new(method_id.to_string()),
        )
        .await?;
        let _: AuthenticateResponse =
            parse(pump_to_response(wire, ID_AUTHENTICATE, sink, seq, resolver).await?)?;
    }
    Ok(init)
}

async fn open_new_session(
    wire: &mut Wire<'_>,
    sink: &mut dyn RunFactAppender,
    seq: &mut u64,
    resolver: &dyn PermissionResolver,
    cwd: &str,
    mcp_servers: &[crate::SessionMcpServer],
) -> Result<NewSessionResponse, AcpError> {
    wire.send_request(
        ID_NEW_SESSION,
        AGENT_METHOD_NAMES.session_new,
        NewSessionRequest::new(cwd).mcp_servers(to_acp_mcp_servers(mcp_servers)),
    )
    .await?;
    parse(pump_to_response(wire, ID_NEW_SESSION, sink, seq, resolver).await?)
}

/// Negotiate one prompt-free ACP Session and retain the full advertised
/// capability descriptors. The caller bounds and reaps the process.
pub async fn negotiate_capabilities(
    channel: &mut dyn AgentChannel,
    config: &AcpCapabilityProbeConfig,
) -> Result<NegotiatedAcpCapabilities, AcpError> {
    struct RejectPermission;
    #[async_trait::async_trait]
    impl PermissionResolver for RejectPermission {
        async fn resolve(&self, _ask: &PermissionAsk) -> PermissionVerdict {
            PermissionVerdict::Deny
        }
    }
    struct DiscardFacts;
    #[async_trait::async_trait]
    impl RunFactAppender for DiscardFacts {
        async fn append(
            &mut self,
            _seq: u64,
            _event: &AcpProjectedEvent,
        ) -> Result<(), AppendError> {
            Ok(())
        }
    }

    let cwd = config.session_cwd.as_deref().unwrap_or("/");
    let resolver = RejectPermission;
    let mut sink = DiscardFacts;
    let mut seq = 0;
    let mut wire = Wire::new(channel);
    let init = initialize_agent(
        &mut wire,
        &mut sink,
        &mut seq,
        &resolver,
        config.auth_method_id.as_deref(),
    )
    .await?;
    let session = open_new_session(&mut wire, &mut sink, &mut seq, &resolver, cwd, &[]).await?;
    Ok(crate::capabilities::project(init, session))
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
    let init = initialize_agent(
        &mut wire,
        sink,
        &mut seq,
        resolver,
        config.auth_method_id.as_deref(),
    )
    .await?;
    let can_load = init.agent_capabilities.load_session;

    // 2. session/load (resume the CLI's own session across the relaunch) when we
    //    hold a prior id and the agent supports it; else session/new. Fail-safe:
    //    an agent that does not advertise `loadSession` falls back to a fresh
    //    session (the neutral thread history is the authority — never lost).
    let (session_id, negotiated): (SessionId, NegotiatedAcpCapabilities) =
        match config.session_id.clone() {
            Some(prior) if can_load => {
                wire.send_request(
                    ID_NEW_SESSION,
                    AGENT_METHOD_NAMES.session_load,
                    LoadSessionRequest::new(SessionId::new(prior.as_str()), cwd.as_str()),
                )
                .await?;
                match pump_response(&mut wire, ID_NEW_SESSION, sink, &mut seq, resolver).await? {
                    RpcResponse::Result(result) => {
                        let resp: LoadSessionResponse = parse(result)?;
                        let negotiated = crate::capabilities::project_parts(
                            init.clone(),
                            resp.modes,
                            resp.config_options,
                        );
                        (SessionId::new(prior.as_str()), negotiated)
                    }
                    RpcResponse::Error(error) if is_missing_session_error(&error) => {
                        // Some agents advertise loadSession but retain ids only for the
                        // lifetime of one ACP process (Kimi Code is one example). No
                        // prompt has been sent yet, so opening a fresh session is a safe
                        // compatibility fallback and cannot replay agent work.
                        let new_session = open_new_session(
                            &mut wire,
                            sink,
                            &mut seq,
                            resolver,
                            &cwd,
                            &config.mcp_servers,
                        )
                        .await?;
                        let session_id = new_session.session_id.clone();
                        let negotiated = crate::capabilities::project(init.clone(), new_session);
                        (session_id, negotiated)
                    }
                    RpcResponse::Error(error) => {
                        return Err(AcpError::Frame(error.to_string()));
                    }
                }
            }
            _ => {
                let new_session = open_new_session(
                    &mut wire,
                    sink,
                    &mut seq,
                    resolver,
                    &cwd,
                    &config.mcp_servers,
                )
                .await?;
                let session_id = new_session.session_id.clone();
                let negotiated = crate::capabilities::project(init.clone(), new_session);
                (session_id, negotiated)
            }
        };
    if let Some(expected) = &config.expected_capability {
        let actual = awaken_acp_contract::capability_fingerprint(
            &expected.adapter_id,
            &expected.adapter_version,
            &negotiated,
        );
        if actual != expected.fingerprint {
            return Err(AcpError::Frame(
                "ACP capability fingerprint changed after publication".into(),
            ));
        }
    }
    let available_modes = negotiated
        .modes
        .iter()
        .map(|mode| mode.native_id.clone())
        .collect::<Vec<_>>();
    let available_config_options = negotiated
        .config_options
        .iter()
        .map(|option| option.native_id.clone())
        .collect::<Vec<_>>();
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

    // 2c. session/set_config_option — exact backend-owned model selection. The
    // option must be advertised by this exact Session and the agent must accept
    // the value; either failure is terminal and never falls back to its default.
    if let Some(selection) = config.session_config_option.clone() {
        if !available_config_options.contains(&selection.config_id) {
            return Err(AcpError::Frame(format!(
                "configured ACP session option `{}` was not advertised",
                selection.config_id
            )));
        }
        wire.send_request(
            ID_SET_CONFIG_OPTION,
            AGENT_METHOD_NAMES.session_set_config_option,
            SetSessionConfigOptionRequest::new(
                session_id.clone(),
                selection.config_id,
                selection.value,
            ),
        )
        .await?;
        let _: SetSessionConfigOptionResponse = parse(
            pump_to_response(&mut wire, ID_SET_CONFIG_OPTION, sink, &mut seq, resolver).await?,
        )?;
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
            &AcpProjectedEvent::Usage {
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
    sink.append(seq, &AcpProjectedEvent::TurnEnd { reason })
        .await?;
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
    match pump_response(wire, target_id, sink, seq, resolver).await? {
        RpcResponse::Result(result) => Ok(result),
        RpcResponse::Error(error) => Err(AcpError::Frame(error.to_string())),
    }
}

enum RpcResponse {
    Result(serde_json::Value),
    Error(serde_json::Value),
}

fn is_missing_session_error(error: &serde_json::Value) -> bool {
    let message = error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    message.contains("unknown session")
        || message.contains("session not found")
        || message.contains("no such session")
}

/// The response pump with a typed JSON-RPC error branch. Most call sites retain
/// fail-closed behavior through [`pump_to_response`]; `session/load` uses the
/// explicit error branch to fall back before any prompt or side effect occurs.
async fn pump_response(
    wire: &mut Wire<'_>,
    target_id: u64,
    sink: &mut dyn RunFactAppender,
    seq: &mut u64,
    resolver: &dyn PermissionResolver,
) -> Result<RpcResponse, AcpError> {
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
                    return Ok(RpcResponse::Error(error));
                }
                return Ok(RpcResponse::Result(
                    msg.result.unwrap_or(serde_json::Value::Null),
                ));
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
        // A provider HARD-quota banner can arrive as assistant TEXT ("You've hit
        // your weekly limit · resets …") rather than a structured error — the case
        // where the CLI then hangs. Consult the detector before committing: a
        // recognized banner fails the turn closed with the classified failure
        // (RateLimited → Error) instead of landing as an ordinary assistant
        // message. Any other text still projects normally below.
        if let AcpProjectedEvent::Message { text } = &event
            && let Some(failure) = crate::streamed_hard_limit(text)
        {
            return Err(AcpError::HardLimit(failure));
        }
        *seq += 1;
        sink.append(*seq, &event).await?;
    }
    Ok(())
}

/// Answer an agent→client request: a permission request is decided by `resolver`
/// (the neutral `ToolPermissionPolicy`) and projected back onto the agent's own
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
                let ask = permission_ask(&raw);
                match resolver.resolve(&ask).await {
                    PermissionVerdict::Await { correlation_id } => {
                        wire.send(&OutResult {
                            jsonrpc: JSONRPC,
                            id,
                            result: RequestPermissionResponse::new(
                                RequestPermissionOutcome::Cancelled,
                            ),
                        })
                        .await?;
                        return Err(AcpError::PermissionAwait {
                            correlation_id,
                            ask,
                        });
                    }
                    verdict => select_outcome(&req, verdict),
                }
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
        PermissionVerdict::Await { .. } => {
            unreachable!("await is handled before immediate outcome selection")
        }
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

    #[derive(Default)]
    struct RecordingSink {
        last: u64,
        events: Vec<(u64, AcpProjectedEvent)>,
    }

    #[async_trait]
    impl RunFactAppender for RecordingSink {
        async fn append(&mut self, seq: u64, event: &AcpProjectedEvent) -> Result<(), AppendError> {
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

    /// A resolver that denies every ask — stands in for a `ToolPermissionPolicy` that
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

    #[tokio::test]
    async fn capability_probe_reuses_the_handshake_and_sends_no_prompt() {
        // Cause graph:
        // C1 initialize capabilities -> E1 neutral protocol flags.
        // C2 session mode state -> E2 ids/labels/current mode.
        // C3 native select option -> E3 category/current/choices retained.
        // C4 prompt-free probe -> E4 exactly initialize + session/new.
        //
        // Decision table:
        // N1 advertised flags/modes/options -> complete neutral descriptors
        // N2 absent optional capability     -> false/empty (schema defaults)
        // N3 probe lifecycle                -> no session/prompt request
        let (mut ours, theirs) = channel();
        let methods = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = methods.clone();
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            let initialize = io.read().await.unwrap();
            observed.lock().unwrap().push(
                initialize["method"]
                    .as_str()
                    .expect("initialize method")
                    .to_string(),
            );
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":true,"embeddedContext":true},"mcpCapabilities":{"http":true}}}}"#,
            )
            .await;
            let new_session = io.read().await.unwrap();
            observed.lock().unwrap().push(
                new_session["method"]
                    .as_str()
                    .expect("session/new method")
                    .to_string(),
            );
            io.write_line(
                r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"probe","modes":{"currentModeId":"code","availableModes":[{"id":"code","name":"Code"},{"id":"plan","name":"Plan","description":"Plan first"}]},"configOptions":[{"id":"reasoning","name":"Reasoning effort","description":"Native effort","category":"thought_level","type":"select","currentValue":"high","options":[{"value":"low","name":"Low"},{"value":"high","name":"High","description":"More reasoning"}]}]}}"#,
            )
            .await;
        });

        let capabilities = negotiate_capabilities(
            ours.as_mut(),
            &AcpCapabilityProbeConfig {
                session_cwd: Some("/probe".into()),
                auth_method_id: None,
            },
        )
        .await
        .expect("N1");
        agent.await.unwrap();

        assert_eq!(
            methods.lock().unwrap().as_slice(),
            &[
                AGENT_METHOD_NAMES.initialize.to_string(),
                AGENT_METHOD_NAMES.session_new.to_string(),
            ],
            "N3"
        );
        assert!(capabilities.load_session && capabilities.prompt_image);
        assert!(capabilities.prompt_embedded_context && capabilities.mcp_http);
        assert!(!capabilities.prompt_audio && !capabilities.mcp_sse, "N2");
        assert_eq!(capabilities.modes.len(), 2);
        assert!(capabilities.modes[0].current, "N1 current mode");
        assert_eq!(capabilities.modes[1].native_id, "plan");
        let option = &capabilities.config_options[0];
        assert_eq!(option.native_id, "reasoning");
        assert_eq!(option.category.as_deref(), Some("thought_level"));
        assert_eq!(option.current_value, "high");
        assert_eq!(option.choices[1].native_value, "high");
        assert_eq!(
            option.choices[1].description.as_deref(),
            Some("More reasoning")
        );
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

    async fn authenticated_agent(side: DuplexStream) {
        let mut io = AgentIo::new(side);
        let initialize = io.read().await.expect("initialize");
        assert_eq!(initialize["method"], AGENT_METHOD_NAMES.initialize);
        io.write_line(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[{"id":"api-key","name":"API key"}]}}"#,
        )
        .await;

        let authenticate = io.read().await.expect("authenticate");
        assert_eq!(authenticate["id"], ID_AUTHENTICATE);
        assert_eq!(authenticate["method"], AGENT_METHOD_NAMES.authenticate);
        assert_eq!(authenticate["params"]["methodId"], "api-key");
        io.write_line(r#"{"jsonrpc":"2.0","id":5,"result":{}}"#)
            .await;

        let new_session = io.read().await.expect("session/new");
        assert_eq!(new_session["method"], AGENT_METHOD_NAMES.session_new);
        io.write_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"authenticated"}}"#)
            .await;

        let prompt = io.read().await.expect("session/prompt");
        assert_eq!(prompt["method"], AGENT_METHOD_NAMES.session_prompt);
        io.write_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
            .await;
    }

    fn channel() -> (Box<dyn AgentChannel>, DuplexStream) {
        let (ours, theirs) = tokio::io::duplex(8192);
        (Box::new(ours), theirs)
    }

    #[tokio::test]
    async fn selects_the_catalog_auth_method_before_opening_a_session() {
        let (mut ours, theirs) = channel();
        let agent = tokio::spawn(authenticated_agent(theirs));
        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.auth_method_id = Some("api-key".to_string());

        let reason = run_turn_with_config(ours.as_mut(), "do it", &mut sink, &mut config, None)
            .await
            .unwrap();
        assert_eq!(reason, TerminationReason::NaturalEnd);
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn configured_auth_method_must_be_advertised() {
        let (mut ours, theirs) = channel();
        let agent = tokio::spawn(scripted_agent(theirs, Vec::new(), "end_turn"));
        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.auth_method_id = Some("api-key".to_string());

        let error = run_turn_with_config(ours.as_mut(), "do it", &mut sink, &mut config, None)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("authentication method `api-key` was not advertised")
        );
        agent.abort();
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
            AcpProjectedEvent::Message { text } if text == "hello from acp"
        ));
        assert!(matches!(
            sink.events.last().unwrap().1,
            AcpProjectedEvent::TurnEnd {
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
                AcpProjectedEvent::Message { text } => Some(text.clone()),
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
                AcpProjectedEvent::ToolCall { name, input, .. } => {
                    Some((name.clone(), input.clone()))
                }
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
    async fn an_awaiting_policy_cancels_the_wire_request_and_surfaces_the_neutral_ask() {
        struct Awaiting;
        #[async_trait]
        impl PermissionResolver for Awaiting {
            async fn resolve(&self, _ask: &PermissionAsk) -> PermissionVerdict {
                PermissionVerdict::Await {
                    correlation_id: "approval-42".to_string(),
                }
            }
        }

        let (mut ours, theirs) = channel();
        let reply = Arc::new(Mutex::new(String::new()));
        let captured = reply.clone();
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
            io.read().await;
            io.write_line(PERM).await;
            io.read().await;
            *captured.lock().unwrap() = io.line.clone();
        });
        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&Awaiting);
        let error = run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .expect_err("await ends this process attempt");
        agent.await.unwrap();

        assert!(reply.lock().unwrap().contains("cancelled"));
        match error {
            AcpError::PermissionAwait {
                correlation_id,
                ask,
            } => {
                assert_eq!(correlation_id, "approval-42");
                assert_eq!(ask.call_id, "t1");
                assert_eq!(ask.tool, "bash");
                assert_eq!(ask.arguments["cmd"], "ls");
            }
            other => panic!("expected PermissionAwait, got {other:?}"),
        }
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
    async fn a_process_local_session_id_falls_back_to_session_new_before_prompt() {
        let (mut ours, theirs) = channel();
        let methods = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured = methods.clone();
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            io.read().await; // initialize
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}"#,
            )
            .await;

            let load = io.read().await.unwrap();
            captured.lock().unwrap().push(
                load.get("method")
                    .and_then(|value| value.as_str())
                    .unwrap()
                    .to_string(),
            );
            io.write_line(
                r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"Invalid params: Unknown sessionId: old"}}"#,
            )
            .await;

            let fresh = io.read().await.unwrap();
            captured.lock().unwrap().push(
                fresh
                    .get("method")
                    .and_then(|value| value.as_str())
                    .unwrap()
                    .to_string(),
            );
            io.write_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"fresh"}}"#)
                .await;

            let prompt = io.read().await.unwrap();
            captured.lock().unwrap().push(
                prompt
                    .get("method")
                    .and_then(|value| value.as_str())
                    .unwrap()
                    .to_string(),
            );
            io.write_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
                .await;
        });

        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.session_id = Some("old".into());
        run_turn_with_config(ours.as_mut(), "continue", &mut sink, &mut config, None)
            .await
            .unwrap();
        assert_eq!(
            methods.lock().unwrap().as_slice(),
            &[
                AGENT_METHOD_NAMES.session_load.to_string(),
                AGENT_METHOD_NAMES.session_new.to_string(),
                AGENT_METHOD_NAMES.session_prompt.to_string(),
            ]
        );
        assert_eq!(config.session_id.as_deref(), Some("fresh"));
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
    async fn an_exact_session_config_option_is_set_before_prompt_or_fails_closed() {
        // Cause graph: published exact model -> advertised Session option ->
        // session/set_config_option response -> prompt. Missing advertisement
        // terminates before either selection fallback or prompt.
        //
        // Decision table:
        // C1 advertised + accepted -> exact configId/value, then prompt
        // C2 not advertised        -> error, no set request and no prompt
        let (mut ours, theirs) = channel();
        let selected = Arc::new(Mutex::new(None));
        let selected_by_agent = selected.clone();
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            io.read().await; // initialize
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            )
            .await;
            io.read().await; // session/new
            io.write_line(
                r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1","configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"default","options":[{"value":"gpt-exact","name":"GPT Exact"}]}]}}"#,
            )
            .await;
            let set = io.read().await.unwrap(); // session/set_config_option (id 6)
            *selected_by_agent.lock().unwrap() = Some(set.clone());
            io.write_line(r#"{"jsonrpc":"2.0","id":6,"result":{"configOptions":[]}}"#)
                .await;
            let prompt = io.read().await.unwrap();
            assert_eq!(
                prompt.get("method").and_then(|value| value.as_str()),
                Some(AGENT_METHOD_NAMES.session_prompt),
                "C1"
            );
            io.write_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
                .await;
        });

        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.session_config_option = Some(crate::SessionConfigOptionSelection {
            config_id: "model".into(),
            value: "gpt-exact".into(),
        });
        run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .expect("C1");
        {
            let selected = selected.lock().unwrap();
            let request = selected.as_ref().expect("C1 set request");
            assert_eq!(
                request.get("method").and_then(|value| value.as_str()),
                Some(AGENT_METHOD_NAMES.session_set_config_option),
                "C1"
            );
            assert_eq!(request["params"]["configId"], "model", "C1");
            assert_eq!(request["params"]["value"], "gpt-exact", "C1");
        }
        agent.await.unwrap();

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
        });
        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.session_config_option = Some(crate::SessionConfigOptionSelection {
            config_id: "model".into(),
            value: "gpt-exact".into(),
        });
        let error = run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .expect_err("C2");
        assert!(error.to_string().contains("was not advertised"), "C2");
        agent.await.unwrap();
    }

    // Cause/effect decision table for the claim-to-launch capability fence:
    // F1 live handshake fingerprint equals publication pin -> continue.
    // F2 any mode/option/protocol difference changes fingerprint -> fail before
    // set_mode, set_config_option, or prompt; no backend/default fallback.
    #[tokio::test]
    async fn capability_fingerprint_matches_or_fails_before_prompt() {
        let live = NegotiatedAcpCapabilities {
            protocol_version: "1".into(),
            load_session: false,
            prompt_image: false,
            prompt_audio: false,
            prompt_embedded_context: false,
            mcp_http: false,
            mcp_sse: false,
            session_list: false,
            modes: Vec::new(),
            config_options: Vec::new(),
        };
        let expected = awaken_acp_contract::capability_fingerprint("codex", "test", &live);
        let (mut ours, theirs) = channel();
        let matching_agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            io.read().await;
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            )
            .await;
            io.read().await;
            io.write_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#)
                .await;
            let prompt = io.read().await.expect("F1 prompt");
            assert_eq!(
                prompt.get("method").and_then(|value| value.as_str()),
                Some(AGENT_METHOD_NAMES.session_prompt),
                "F1"
            );
            io.write_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
                .await;
        });
        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.expected_capability = Some(crate::AcpCapabilityExpectation {
            adapter_id: "codex".into(),
            adapter_version: "test".into(),
            fingerprint: expected,
        });
        run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .expect("F1");
        matching_agent.await.unwrap();

        let (mut ours, theirs) = channel();
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            io.read().await; // initialize
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            )
            .await;
            io.read().await; // session/new
            io.write_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#)
                .await;
            assert!(
                io.read().await.is_none(),
                "F2: no configuration or prompt follows a mismatched handshake"
            );
        });
        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.expected_capability = Some(crate::AcpCapabilityExpectation {
            adapter_id: "codex".into(),
            adapter_version: "test".into(),
            fingerprint: "sha256:not-the-live-profile".into(),
        });
        let error = run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .expect_err("F2");
        assert!(error.to_string().contains("fingerprint"), "F2");
        drop(ours);
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
                AcpProjectedEvent::Usage {
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
        let shape: Vec<AcpProjectedEvent> = sink.events.iter().map(|(_, e)| e.clone()).collect();
        assert_eq!(
            shape,
            vec![
                AcpProjectedEvent::Message {
                    text: "Hello ".into()
                },
                AcpProjectedEvent::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": "a.txt" }),
                },
                AcpProjectedEvent::Message {
                    text: "world".into()
                },
                AcpProjectedEvent::TurnEnd {
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
                AcpProjectedEvent::Message { text } if text == "past the stale id"
            )),
            "the message after the stale id still projected: {:?}",
            sink.events
        );
        agent.await.unwrap();
    }

    // ── `streamed_hard_limit` is wired into the streaming driver ──────────────

    #[tokio::test]
    async fn a_quota_banner_streamed_as_assistant_text_is_failed_closed() {
        // `error::streamed_hard_limit` detects a provider's HARD quota banner that
        // arrives as assistant TEXT ("You've hit your weekly limit · resets …") — the
        // case where the CLI then hangs — and classifies it RateLimited. The streaming
        // driver now consults it: such a banner fails the turn CLOSED (a `HardLimit`
        // error carrying the RateLimited failure → Error termination) instead of being
        // committed as an ordinary assistant message.
        let banner = "You've hit your weekly limit · resets Jun 30";
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
        let err = run_turn(ours.as_mut(), "p", &mut sink, None)
            .await
            .unwrap_err();
        // Failed closed: a HardLimit error carrying a RateLimited failure that maps to
        // an Error termination (the CLI is not left to hang on the banner).
        match err {
            AcpError::HardLimit(failure) => {
                assert_eq!(
                    failure.class,
                    crate::AcpFailureClass::RateLimited {
                        retry_after_secs: None
                    }
                );
                assert_eq!(failure.termination(), TerminationReason::Error);
            }
            other => panic!("expected AcpError::HardLimit, got {other:?}"),
        }
        // The banner is NOT committed as a plain assistant message.
        assert!(
            !sink.events.iter().any(|(_, e)| matches!(
                e,
                AcpProjectedEvent::Message { text } if text == banner
            )),
            "the banner is not projected as plain assistant text: {:?}",
            sink.events
        );
        agent.abort();
    }
}

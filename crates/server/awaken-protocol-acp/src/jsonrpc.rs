// @formal entrypoint acp.prompt_is_never_sent_before_bounded_handshake_completion
// @formal entrypoint acp.session_load_preserves_the_exact_mcp_projection
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
    AGENT_METHOD_NAMES, CLIENT_METHOD_NAMES, PermissionOptionKind, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionId, SessionNotification,
};
#[cfg(test)]
use awaken_acp_contract::NegotiatedAcpCapabilities;
use awaken_agent_channel::AgentChannel;
use serde::Serialize;

mod handshake;
mod mcp;
mod permission;
mod response;
mod wire;
pub use handshake::negotiate_capabilities;
use handshake::{HandshakeProgress, handshake_step, initialize_agent};
use wire::{JSONRPC, Wire};

use mcp::to_acp_mcp_servers;
use permission::{PermissionContext, RejectPermission, answer_request};
use response::{RpcResponse, is_missing_session_error, pump_response, pump_to_response};

#[cfg(test)]
use crate::AppendError;
use crate::real_acp::termination_from_stop_reason;
use crate::{
    AcpError, AcpLaunchEvent, AcpLaunchStage, AcpProjectedEvent, AllowAll, LaunchSink,
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
/// Exact managed model selection through the ACP Session interface.
const ID_SET_MODEL: u64 = 6;
/// Exact backend-owned config option selection after opening the Session.
const ID_SET_CONFIG_OPTION: u64 = 7;
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
    let mut wire = Wire::new(channel);
    let mut seq = 0u64;
    let progress = HandshakeProgress::started();

    // The process is up; the ACP handshake (initialize + session/new|load) begins.
    notify_launch(
        launch_sink,
        AcpLaunchEvent::with_detail(AcpLaunchStage::Initializing, "initialize"),
    );

    // 1. initialize — advertise the latest protocol version and no fs/terminal
    //    capabilities (default `ClientCapabilities`), so those agent requests are
    //    refused fail-closed later. Read back the agent's capabilities to gate a
    //    session resume fail-closed (only load when the agent advertises it).
    let init = handshake_step(
        config.handshake_step_deadline,
        "initialize",
        initialize_agent(
            &mut wire,
            sink,
            &mut seq,
            resolver,
            config.auth_method_id.as_deref(),
        ),
    )
    .await?;
    let progress = progress.initialized()?;

    let (session_id, negotiated, supports_session_model) =
        handshake::open_or_restore_session(&mut wire, sink, &mut seq, config, init, launch_sink)
            .await?;
    let progress = progress.session_opened(supports_session_model)?;
    let prompt_ready = handshake::configure_session(
        &mut wire,
        sink,
        &mut seq,
        config,
        &session_id,
        &negotiated,
        progress,
    )
    .await?;

    // Handshake complete — the agent is live and about to accept the prompt.
    notify_launch(
        launch_sink,
        AcpLaunchEvent::with_detail(AcpLaunchStage::Ready, "session/prompt"),
    );

    // 3. session/prompt — the user turn as one text content block.
    wire.send_request(
        ID_PROMPT,
        AGENT_METHOD_NAMES.session_prompt,
        prompt_ready.request(session_id.clone(), prompt),
    )
    .await?;
    let prompt_result = response::pump_prompt(
        &mut wire,
        &session_id,
        sink,
        &mut seq,
        resolver,
        &config.tool_aliases,
        config.permission_settle_deadline,
    )
    .await?;
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

fn parse<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, AcpError> {
    serde_json::from_value(value).map_err(|e| AcpError::Frame(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_acp_contract::AcpCapabilityProbeConfig;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    #[derive(Default)]
    pub(super) struct RecordingSink {
        last: u64,
        pub(super) events: Vec<(u64, AcpProjectedEvent)>,
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

    /// A resolver that records the ask it saw, then allows — proves the driver
    /// projects the agent's request into a neutral [`PermissionAsk`].
    #[derive(Default)]
    pub(super) struct RecordingResolver {
        pub(super) seen: Mutex<Vec<PermissionAsk>>,
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
    pub(super) struct AgentIo {
        reader: BufReader<DuplexStream>,
        line: String,
    }

    impl AgentIo {
        pub(super) fn new(side: DuplexStream) -> Self {
            Self {
                reader: BufReader::new(side),
                line: String::new(),
            }
        }

        /// Read one JSON line, or `None` at EOF.
        pub(super) async fn read(&mut self) -> Option<serde_json::Value> {
            self.line.clear();
            let n = self.reader.read_line(&mut self.line).await.unwrap();
            (n != 0).then(|| serde_json::from_str(self.line.trim()).unwrap())
        }

        pub(super) async fn write_line(&mut self, line: &str) {
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

    #[tokio::test]
    async fn every_handshake_failure_is_bounded_and_precedes_prompt() {
        // Causes: C1 each required request boundary (initialize, optional auth,
        // new/load, missing-load fallback, mode, model, and either exact option)
        // receives an explicit error; C2 the same boundary instead remains open
        // without a response. Effects: E1 C1 fails closed; E2 C2 returns the
        // boundary's deadline error; E3 neither outcome sends session/prompt,
        // emits Ready, or projects a prompt-side fact. Constraints: one production
        // driver, Wire, and handshake_step own every row; the missing-Session error
        // alone authorizes new-session fallback; selected configuration order is
        // mode -> model -> option[0] -> option[1]. Decision rules H1=C1=>E1+E3
        // and H2=C2=>E2+E3, crossed with all nine boundaries below.
        let cases = [
            ("initialize", false, false, 0, "initialize"),
            ("authenticate", false, false, 0, "initialize"),
            ("session/new", false, false, 0, "session/new"),
            ("session/load", true, false, 0, "session/load"),
            ("session/new", true, true, 0, "session/new"),
            ("session/set_mode", false, false, 0, "session/set_mode"),
            ("session/set_model", false, false, 0, "session/set_model"),
            (
                "session/set_config_option",
                false,
                false,
                0,
                "session/set_config_option",
            ),
            (
                "session/set_config_option",
                false,
                false,
                1,
                "session/set_config_option",
            ),
        ];
        for (target, prior, missing, target_index, stage) in cases {
            for stalls in [false, true] {
                let (mut ours, theirs) = channel();
                let methods = Arc::new(Mutex::new(Vec::new()));
                let observed = methods.clone();
                let adapter = tokio::spawn(async move {
                    let mut io = AgentIo::new(theirs);
                    let mut target_seen = 0;
                    while let Some(request) = io.read().await {
                        let method = request["method"].as_str().unwrap().to_string();
                        observed.lock().unwrap().push(method.clone());
                        if method == target && target_seen == target_index {
                            if stalls {
                                std::future::pending::<()>().await;
                            }
                            io.write_line(
                                &serde_json::json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32001,"message":"forced handshake failure"}}).to_string(),
                            )
                            .await;
                            return;
                        }
                        target_seen += usize::from(method == target);
                        let response = match method.as_str() {
                            "initialize" => {
                                serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":prior},"authMethods":[{"id":"api-key","name":"API key"}]}})
                            }
                            "authenticate" => {
                                assert_eq!(request["params"]["methodId"], "api-key");
                                serde_json::json!({"jsonrpc":"2.0","id":5,"result":{}})
                            }
                            "session/load" if missing => {
                                serde_json::json!({"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"Unknown sessionId"}})
                            }
                            "session/load" => {
                                unreachable!("load succeeds only after the target row")
                            }
                            "session/new" => {
                                serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1","modes":{"currentModeId":"plan","availableModes":[{"id":"plan","name":"Plan"}]},"models":{"currentModelId":"default","availableModels":[]},"configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"default","options":[]},{"id":"reasoning","name":"Reasoning","type":"select","currentValue":"low","options":[]}]}})
                            }
                            "session/set_mode" => {
                                serde_json::json!({"jsonrpc":"2.0","id":4,"result":{}})
                            }
                            "session/set_model" => {
                                serde_json::json!({"jsonrpc":"2.0","id":6,"result":{}})
                            }
                            "session/set_config_option" => {
                                serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{"configOptions":[]}})
                            }
                            "session/prompt" => {
                                panic!("H1/H2/E3 prompt preceded handshake completion")
                            }
                            other => panic!("unexpected method {other}"),
                        };
                        io.write_line(&response.to_string()).await;
                    }
                });
                let resolver = RecordingResolver::default();
                let mut config = TurnConfig::new(&resolver);
                config.auth_method_id = Some("api-key".into());
                config.session_id = prior.then(|| "old".into());
                config.session_mode = Some("plan".into());
                config.session_model = Some("provider/exact".into());
                config.session_config_options = ["model", "reasoning"]
                    .map(|id| crate::SessionConfigOptionSelection {
                        config_id: id.into(),
                        value: "exact".into(),
                    })
                    .to_vec();
                config.handshake_step_deadline = std::time::Duration::from_millis(20);
                let launch = crate::tests::RecordingLaunch::default();
                let mut sink = RecordingSink::default();
                let error = run_turn_with_config(
                    ours.as_mut(),
                    "not sent",
                    &mut sink,
                    &mut config,
                    Some(LaunchSink::new(&launch, "bounded-handshake")),
                )
                .await
                .expect_err("H1/H2 handshake must fail");
                if stalls {
                    assert!(
                        matches!(&error, AcpError::Io(detail) if detail.contains(&format!("{stage} handshake timed out"))),
                        "H2/E2: {error}"
                    );
                    adapter.abort();
                } else {
                    assert!(matches!(error, AcpError::Frame(_)), "H1/E1: {error}");
                    adapter.await.unwrap();
                }
                let observed = methods.lock().unwrap();
                assert!(
                    !observed.iter().any(|method| method == "session/prompt"),
                    "H1/H2/E3"
                );
                if target == "session/set_config_option" && target_index == 1 {
                    assert_eq!(
                        observed.as_slice(),
                        [
                            "initialize",
                            "authenticate",
                            "session/new",
                            "session/set_mode",
                            "session/set_model",
                            "session/set_config_option",
                            "session/set_config_option"
                        ],
                        "H1/H2 selected configuration preserves its exact order"
                    );
                }
                assert!(
                    launch
                        .events
                        .lock()
                        .unwrap()
                        .iter()
                        .all(|(_, event)| event.stage == AcpLaunchStage::Initializing),
                    "H1/H2/E3"
                );
                assert!(sink.events.is_empty(), "H1/H2/E3");
            }
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

    pub(super) fn channel() -> (Box<dyn AgentChannel>, DuplexStream) {
        let (ours, theirs) = tokio::io::duplex(8192);
        (Box::new(ours), theirs)
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
        // L1 each completed handshake step emits the next protocol boundary
        // through the existing observer. L2 Ready precedes prompt admission;
        // it must not claim an inference response or terminal business result.
        let launch = crate::tests::RecordingLaunch::default();
        let reason = run_turn(
            ours.as_mut(),
            "do it",
            &mut sink,
            Some(LaunchSink::new(&launch, "launch-test")),
        )
        .await
        .unwrap();
        assert_eq!(
            launch
                .events
                .lock()
                .unwrap()
                .iter()
                .map(|(_, event)| (event.stage, event.detail.clone()))
                .collect::<Vec<_>>(),
            vec![
                (AcpLaunchStage::Initializing, Some("initialize".into())),
                (AcpLaunchStage::Initializing, Some("session/new".into())),
                (AcpLaunchStage::Ready, Some("session/prompt".into())),
            ]
        );
        assert_eq!(reason, TerminationReason::NaturalEnd);
        // One projected message + the synthetic TurnEnd, in seq order.
        assert!(matches!(
            &sink.events[0].1,
            AcpProjectedEvent::Message { text, .. } if text == "hello from acp"
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
                AcpProjectedEvent::Message { text, .. } => Some(text.clone()),
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
    async fn drive_permission(
        resolver: &dyn PermissionResolver,
        notifications: Vec<String>,
        permission: String,
    ) -> (String, RecordingSink) {
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
            for notification in notifications {
                io.write_line(&notification).await;
            }
            io.write_line(&permission).await;
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

        (seen.lock().unwrap().clone(), sink)
    }

    #[tokio::test]
    async fn a_denying_policy_selects_the_agents_reject_option() {
        // The neutral policy denies → we select the agent's own reject option.
        let (reply, _) = drive_permission(&RejectPermission, vec![], PERM.into()).await;
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
        let (reply, _) = drive_permission(&resolver, vec![], PERM.into()).await;
        assert!(reply.contains("selected"), "selected an option: {reply}");
        assert!(reply.contains("\"ok\""), "the allow option id: {reply}");
        let seen = resolver.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "the resolver saw exactly one ask");
        assert_eq!(seen[0].tool, "bash");
        assert_eq!(seen[0].call_id, "t1");
        assert_eq!(seen[0].arguments["cmd"], "ls");
    }

    #[tokio::test]
    async fn resolved_permission_refines_the_same_streamed_call_before_execution() {
        // P1 alias stream title + exact resolved permission -> both policy and
        // transcript use the resolved target/complete input. P2 denied requests
        // still project the exact requested operation without permitting execution.
        // Existing Await test covers P3 durable suspension with the same ask.
        for allow in [true, false] {
            let resolver = RecordingResolver::default();
            let target = "mcp__awaken_session__read";
            let notification = serde_json::json!({"jsonrpc":"2.0","method":"session/update",
                "params":{"sessionId":"sess-1","update":{"sessionUpdate":"tool_call",
                    "toolCallId":"t1","title":"Read File","rawInput":{}}}})
            .to_string();
            let mut permission: serde_json::Value = serde_json::from_str(PERM).unwrap();
            permission["params"]["toolCall"]["title"] = target.into();
            let (reply, sink) = drive_permission(
                if allow { &resolver } else { &RejectPermission },
                vec![notification],
                permission.to_string(),
            )
            .await;
            assert!(reply.contains(if allow { "\"ok\"" } else { "\"no\"" }));
            if allow {
                assert_eq!(resolver.seen.lock().unwrap()[0].tool, target);
            }
            let calls: Vec<_> = sink
                .events
                .iter()
                .filter_map(|(_, event)| match event {
                    AcpProjectedEvent::ToolCall { id, name, input } => Some((id, name, input)),
                    _ => None,
                })
                .collect();
            assert_eq!(calls.len(), 2, "one placeholder and one refinement");
            assert_eq!(calls[0].0, calls[1].0, "same operation");
            assert_eq!(calls[1].1, target);
            assert_eq!(calls[1].2["cmd"], "ls");
        }
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
        // Cause/effect graph: C1 a prior Session id exists, C2 loadSession is
        // advertised, and C3 the Host supplies an MCP projection. Together they
        // must produce E1 session/load (not session/new), E2 the exact MCP routes
        // on that load request, E3 the same durable Session id after the turn,
        // and E4 no replayed historical notification in the current Run facts.
        // This is the relaunch path used after HITL approval; omitting E2 makes a
        // loaded Codex Session retain conversation context but lose its tools.
        // FMECA FM1: committing load replay duplicates transcript/tool facts and
        // can make old instructions look current; the load-only discard appender
        // mitigates it while the external Session retains its own history.
        //
        // | Rule | Prior id | Can load | MCP configured | Effect |
        // | R1 | yes | yes | yes | load + exact MCP set + retain id + no replay |
        // | R2 | yes | no | any | session/new (covered below) |
        // | R3 | any | any | no | empty MCP set (default-path coverage) |
        let (mut ours, theirs) = channel();
        let saw_load = Arc::new(Mutex::new(false));
        let loaded_params = Arc::new(Mutex::new(serde_json::Value::Null));
        let saw_load2 = saw_load.clone();
        let loaded_params2 = loaded_params.clone();
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
            *loaded_params2.lock().unwrap() = req2
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            io.write_line(r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-resume","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"historical replay"}}}}"#)
                .await;
            io.write_line(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#)
                .await;
            io.read().await; // prompt
            io.write_line(r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-resume","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"current response"}}}}"#)
                .await;
            io.write_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
                .await;
        });

        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.session_id = Some("sess-resume".into());
        config.mcp_servers = vec![crate::SessionMcpServer {
            name: "pilot".into(),
            command: None,
            args: Vec::new(),
            url: Some("http://pilot.internal/mcp".into()),
            auth: None,
        }];
        let reason = run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .unwrap();
        assert_eq!(reason, TerminationReason::NaturalEnd);
        assert!(*saw_load.lock().unwrap(), "the driver sent session/load");
        assert_eq!(
            loaded_params.lock().unwrap()["mcpServers"][0]["name"],
            "pilot",
            "R1: the exact Host MCP projection reaches session/load"
        );
        assert_eq!(
            loaded_params.lock().unwrap()["mcpServers"][0]["url"],
            "http://pilot.internal/mcp",
            "R1"
        );
        assert_eq!(
            config.session_id.as_deref(),
            Some("sess-resume"),
            "the resumed id is reported back for the next turn"
        );
        let messages = sink
            .events
            .iter()
            .filter_map(|(_, event)| match event {
                AcpProjectedEvent::Message { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(messages, ["current response"], "E4");
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
    async fn an_exact_session_model_is_set_before_prompt_or_fails_closed() {
        // Decision table: advertised model interface -> select exact id then
        // prompt; absent interface -> terminal error before prompt.
        let (mut ours, theirs) = channel();
        let selected = Arc::new(Mutex::new(None));
        let selected_by_agent = selected.clone();
        let agent = tokio::spawn(async move {
            let mut io = AgentIo::new(theirs);
            io.read().await;
            io.write_line(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            )
            .await;
            io.read().await;
            io.write_line(
                r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1","models":{"currentModelId":"default","availableModels":[]}}}"#,
            )
            .await;
            let set = io.read().await.unwrap();
            *selected_by_agent.lock().unwrap() = set
                .get("params")
                .and_then(|params| params.get("modelId"))
                .and_then(|value| value.as_str())
                .map(str::to_string);
            assert_eq!(
                set.get("method").and_then(|value| value.as_str()),
                Some(AGENT_METHOD_NAMES.session_set_model)
            );
            io.write_line(r#"{"jsonrpc":"2.0","id":6,"result":{}}"#)
                .await;
            let prompt = io.read().await.unwrap();
            assert_eq!(
                prompt.get("method").and_then(|value| value.as_str()),
                Some(AGENT_METHOD_NAMES.session_prompt)
            );
            io.write_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
                .await;
        });
        let mut sink = RecordingSink::default();
        let mut config = TurnConfig::new(&AllowAll);
        config.session_model = Some("provider/exact-model".into());
        run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .unwrap();
        assert_eq!(
            selected.lock().unwrap().as_deref(),
            Some("provider/exact-model")
        );
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
        config.session_model = Some("provider/exact-model".into());
        let error = run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .expect_err("missing model interface must fail closed");
        assert!(error.to_string().contains("was not advertised"));
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn an_exact_session_config_option_is_set_before_prompt_or_fails_closed() {
        // Cause graph: published exact model -> advertised Session option ->
        // session/set_config_option response -> prompt. Missing advertisement
        // terminates before either selection fallback or prompt.
        //
        // Decision table:
        // C1 multiple advertised + accepted -> stable exact values, then prompt
        // C2 not advertised        -> error, no set request and no prompt
        let (mut ours, theirs) = channel();
        let selected = Arc::new(Mutex::new(Vec::new()));
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
                r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1","configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"default","options":[{"value":"gpt-exact","name":"GPT Exact"}]},{"id":"reasoning_effort","name":"Reasoning","type":"select","currentValue":"medium","options":[{"value":"high","name":"High"}]}]}}"#,
            )
            .await;
            let set = io.read().await.unwrap(); // first session/set_config_option (id 7)
            selected_by_agent.lock().unwrap().push(set);
            io.write_line(r#"{"jsonrpc":"2.0","id":7,"result":{"configOptions":[]}}"#)
                .await;
            let set = io.read().await.unwrap(); // second session/set_config_option (id 8)
            selected_by_agent.lock().unwrap().push(set);
            io.write_line(r#"{"jsonrpc":"2.0","id":8,"result":{"configOptions":[]}}"#)
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
        config.session_config_options = vec![
            crate::SessionConfigOptionSelection {
                config_id: "model".into(),
                value: "gpt-exact".into(),
            },
            crate::SessionConfigOptionSelection {
                config_id: "reasoning_effort".into(),
                value: "high".into(),
            },
        ];
        run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .expect("C1");
        {
            let selected = selected.lock().unwrap();
            let request = selected.first().expect("C1 first set request");
            assert_eq!(
                request.get("method").and_then(|value| value.as_str()),
                Some(AGENT_METHOD_NAMES.session_set_config_option),
                "C1"
            );
            assert_eq!(request["params"]["configId"], "model", "C1");
            assert_eq!(request["params"]["value"], "gpt-exact", "C1");
            let request = selected.get(1).expect("C1 second set request");
            assert_eq!(request["id"], 8, "C1 unique request id");
            assert_eq!(request["params"]["configId"], "reasoning_effort", "C1");
            assert_eq!(request["params"]["value"], "high", "C1");
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
        config.session_config_options = vec![crate::SessionConfigOptionSelection {
            config_id: "model".into(),
            value: "gpt-exact".into(),
        }];
        let error = run_turn_with_config(ours.as_mut(), "p", &mut sink, &mut config, None)
            .await
            .expect_err("C2");
        assert!(error.to_string().contains("was not advertised"), "C2");
        agent.await.unwrap();
    }

    // Cause/effect decision table for the claim-to-launch capability fence:
    // F1 live handshake fingerprint equals publication pin -> continue.
    // F2 a protocol-level capability difference changes the fingerprint -> fail
    // before set_mode, set_config_option, or prompt. Route-derived mode/option
    // catalogues are validated against the frozen selections below instead of
    // perturbing this placement fence; no backend/default fallback exists.
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
                    text: "Hello ".into(),
                    message_id: None,
                },
                AcpProjectedEvent::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": "a.txt" }),
                },
                AcpProjectedEvent::Message {
                    text: "world".into(),
                    message_id: None,
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
                AcpProjectedEvent::Message { text, .. } if text == "past the stale id"
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
                AcpProjectedEvent::Message { text, .. } if text == banner
            )),
            "the banner is not projected as plain assistant text: {:?}",
            sink.events
        );
        agent.abort();
    }
}

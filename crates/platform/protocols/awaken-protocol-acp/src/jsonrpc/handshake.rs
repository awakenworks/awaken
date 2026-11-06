// @formal entrypoint acp.prompt_is_never_sent_before_bounded_handshake_completion
//! ACP initialize/authenticate/session setup shared by turns and capability probes.

use agent_client_protocol::{
    AGENT_METHOD_NAMES, AuthenticateRequest, AuthenticateResponse, ClientCapabilities,
    ContentBlock, InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
    PromptRequest, ProtocolVersion, SessionId,
};
use awaken_acp_contract::{AcpCapabilityProbeConfig, NegotiatedAcpCapabilities};
use awaken_agent_channel::AgentChannel;

use super::{
    AcpError, ID_AUTHENTICATE, ID_INITIALIZE, ID_NEW_SESSION, PermissionResolver, RejectPermission,
    RunFactAppender, Wire, parse, pump_to_response, to_acp_mcp_servers,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandshakePhase {
    Started,
    Initialized,
    SessionOpened,
    Configured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandshakeStep {
    Initialize,
    OpenSession,
    Configure,
}

const fn advance_handshake(phase: HandshakePhase, step: HandshakeStep) -> Option<HandshakePhase> {
    match (phase, step) {
        (HandshakePhase::Started, HandshakeStep::Initialize) => Some(HandshakePhase::Initialized),
        (HandshakePhase::Initialized, HandshakeStep::OpenSession) => {
            Some(HandshakePhase::SessionOpened)
        }
        (HandshakePhase::SessionOpened, HandshakeStep::Configure) => {
            Some(HandshakePhase::Configured)
        }
        _ => None,
    }
}

const fn prompt_is_admitted(phase: HandshakePhase) -> bool {
    matches!(phase, HandshakePhase::Configured)
}

/// The single turn driver's prompt-free setup state. A phase advances only
/// after the corresponding bounded operation has returned successfully.
#[derive(Debug)]
pub(super) struct HandshakeProgress {
    phase: HandshakePhase,
    supports_session_model: bool,
}

impl HandshakeProgress {
    pub(super) const fn started() -> Self {
        Self {
            phase: HandshakePhase::Started,
            supports_session_model: false,
        }
    }

    pub(super) fn initialized(self) -> Result<Self, AcpError> {
        self.advance(HandshakeStep::Initialize)
    }

    pub(super) fn session_opened(self, supports_session_model: bool) -> Result<Self, AcpError> {
        let mut progress = self.advance(HandshakeStep::OpenSession)?;
        progress.supports_session_model = supports_session_model;
        Ok(progress)
    }

    fn configured(self) -> Result<Self, AcpError> {
        self.advance(HandshakeStep::Configure)
    }

    fn advance(self, step: HandshakeStep) -> Result<Self, AcpError> {
        advance_handshake(self.phase, step)
            .map(|phase| Self {
                phase,
                supports_session_model: self.supports_session_model,
            })
            .ok_or_else(|| AcpError::Frame("ACP handshake phase transition rejected".into()))
    }

    fn into_prompt_ready(self) -> Result<PromptReady, AcpError> {
        if prompt_is_admitted(self.phase) {
            Ok(PromptReady { _private: () })
        } else {
            Err(AcpError::Frame(
                "ACP prompt rejected before handshake completion".into(),
            ))
        }
    }
}

/// An unforgeable-in-production witness that every prompt-free setup phase
/// completed. It is consumed to construct the only production prompt request.
pub(super) struct PromptReady {
    _private: (),
}

impl PromptReady {
    pub(super) fn request(self, session_id: SessionId, prompt: &str) -> PromptRequest {
        PromptRequest::new(session_id, vec![ContentBlock::from(prompt)])
    }
}

pub(super) async fn initialize_agent(
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

pub(super) struct NewSessionConfig<'a> {
    pub cwd: &'a str,
    pub mcp_servers: &'a [crate::SessionMcpServer],
    pub mcp_execution_context: Option<&'a awaken_acp_contract::McpExecutionContext>,
    pub meta: Option<serde_json::Map<String, serde_json::Value>>,
}

pub(super) async fn open_new_session(
    wire: &mut Wire<'_>,
    sink: &mut dyn RunFactAppender,
    seq: &mut u64,
    resolver: &dyn PermissionResolver,
    config: NewSessionConfig<'_>,
) -> Result<NewSessionResponse, AcpError> {
    wire.send_request(
        ID_NEW_SESSION,
        AGENT_METHOD_NAMES.session_new,
        NewSessionRequest::new(config.cwd)
            .mcp_servers(to_acp_mcp_servers(
                config.mcp_servers,
                config.mcp_execution_context,
            ))
            .meta(config.meta),
    )
    .await?;
    parse(pump_to_response(wire, ID_NEW_SESSION, sink, seq, resolver).await?)
}

/// Pinned Claude adapter extension. Disable builtin objects as well as redirecting
/// model-emitted names; this does not grant authority or convert tool arguments.
/// Open or restore the external Session through the one shared handshake owner.
/// Historical replay stays discarded; new/load/recovery use identical metadata.
pub(super) async fn open_or_restore_session(
    wire: &mut Wire<'_>,
    sink: &mut dyn RunFactAppender,
    seq: &mut u64,
    config: &crate::TurnConfig<'_>,
    init: InitializeResponse,
    launch_sink: Option<super::LaunchSink<'_>>,
) -> Result<(SessionId, NegotiatedAcpCapabilities, bool), AcpError> {
    use super::{RpcResponse, is_missing_session_error, pump_response};
    use crate::{AcpLaunchEvent, AcpLaunchStage, notify_launch};
    use agent_client_protocol::{LoadSessionRequest, LoadSessionResponse};
    let cwd = config
        .session_cwd
        .clone()
        .unwrap_or_else(|| "/".to_string());
    let can_load = init.agent_capabilities.load_session;
    let resolver = config.resolver;
    // 2. session/load (resume the CLI's own session across the relaunch) when we
    //    hold a prior id and the agent supports it; else session/new. Fail-safe:
    //    an agent that does not advertise `loadSession` falls back to a fresh
    //    session (the neutral thread history is the authority — never lost).
    let (session_id, negotiated, supports_session_model): (
        SessionId,
        NegotiatedAcpCapabilities,
        bool,
    ) = match config.session_id.clone() {
        Some(prior) if can_load => {
            notify_launch(
                launch_sink,
                AcpLaunchEvent::with_detail(AcpLaunchStage::Initializing, "session/load"),
            );
            // `session/load` may replay the external Agent's entire historical
            // notification stream. Those facts were committed by earlier Runs;
            // the neutral Thread log is their sole owner, so recovery must not
            // append them again under the current Run's ids.
            let mut replay = crate::DiscardRunFacts;
            let load = handshake_step(config.handshake_step_deadline, "session/load", async {
                wire.send_request(
                    ID_NEW_SESSION,
                    AGENT_METHOD_NAMES.session_load,
                    LoadSessionRequest::new(SessionId::new(prior.as_str()), cwd.as_str())
                        .mcp_servers(to_acp_mcp_servers(
                            &config.mcp_servers,
                            config.mcp_execution_context.as_ref(),
                        ))
                        .meta(claude_session_meta(
                            &config.tool_aliases,
                            config.max_turn_requests,
                            config.system_prompt.as_ref(),
                        )),
                )
                .await?;
                pump_response(wire, ID_NEW_SESSION, &mut replay, seq, resolver).await
            })
            .await?;
            match load {
                RpcResponse::Result(result) => {
                    let resp: LoadSessionResponse = parse(result)?;
                    let supports_session_model = resp.models.is_some();
                    let negotiated = crate::capabilities::project_parts(
                        init.clone(),
                        resp.modes,
                        resp.config_options,
                    );
                    (
                        SessionId::new(prior.as_str()),
                        negotiated,
                        supports_session_model,
                    )
                }
                RpcResponse::Error(error) if is_missing_session_error(&error) => {
                    // Some agents advertise loadSession but retain ids only for the
                    // lifetime of one ACP process (Kimi Code is one example). No
                    // prompt has been sent yet, so opening a fresh session is a safe
                    // compatibility fallback and cannot replay agent work.
                    notify_launch(
                        launch_sink,
                        AcpLaunchEvent::with_detail(AcpLaunchStage::Initializing, "session/new"),
                    );
                    let new_session = handshake_step(
                        config.handshake_step_deadline,
                        "session/new",
                        open_new_session(
                            wire,
                            sink,
                            seq,
                            resolver,
                            NewSessionConfig {
                                cwd: &cwd,
                                mcp_servers: &config.mcp_servers,
                                mcp_execution_context: config.mcp_execution_context.as_ref(),
                                meta: claude_session_meta(
                                    &config.tool_aliases,
                                    config.max_turn_requests,
                                    config.system_prompt.as_ref(),
                                ),
                            },
                        ),
                    )
                    .await?;
                    let session_id = new_session.session_id.clone();
                    let supports_session_model = new_session.models.is_some();
                    let negotiated = crate::capabilities::project(init.clone(), new_session);
                    (session_id, negotiated, supports_session_model)
                }
                RpcResponse::Error(error) => {
                    return Err(AcpError::Frame(error.to_string()));
                }
            }
        }
        _ => {
            notify_launch(
                launch_sink,
                AcpLaunchEvent::with_detail(AcpLaunchStage::Initializing, "session/new"),
            );
            let new_session = handshake_step(
                config.handshake_step_deadline,
                "session/new",
                open_new_session(
                    wire,
                    sink,
                    seq,
                    resolver,
                    NewSessionConfig {
                        cwd: &cwd,
                        mcp_servers: &config.mcp_servers,
                        mcp_execution_context: config.mcp_execution_context.as_ref(),
                        meta: claude_session_meta(
                            &config.tool_aliases,
                            config.max_turn_requests,
                            config.system_prompt.as_ref(),
                        ),
                    },
                ),
            )
            .await?;
            let session_id = new_session.session_id.clone();
            let supports_session_model = new_session.models.is_some();
            let negotiated = crate::capabilities::project(init.clone(), new_session);
            (session_id, negotiated, supports_session_model)
        }
    };
    Ok((session_id, negotiated, supports_session_model))
}

/// `_meta` for `session/new|load|resume`. Every adapter receives the same
/// object; adapters that do not read a key ignore it. `systemPrompt` is the
/// top-level claude-agent-acp route and is never duplicated under
/// `claudeCode.options`, where the SDK option would silently win.
pub(super) fn claude_session_meta(
    aliases: &std::collections::BTreeMap<String, String>,
    max_turns: Option<usize>,
    system_prompt: Option<&serde_json::Value>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if aliases.is_empty() && max_turns.is_none() && system_prompt.is_none() {
        return None;
    }
    let mut options = serde_json::Map::new();
    if !aliases.is_empty() {
        options.insert("toolAliases".into(), serde_json::json!(aliases));
        options.insert("tools".into(), serde_json::json!([]));
        options.insert(
            "disallowedTools".into(),
            serde_json::json!(aliases.keys().collect::<Vec<_>>()),
        );
    }
    if let Some(max_turns) = max_turns {
        options.insert("maxTurns".into(), serde_json::json!(max_turns));
    }
    let mut meta = serde_json::Map::new();
    if !options.is_empty() {
        meta.insert(
            "claudeCode".into(),
            serde_json::json!({ "options": options }),
        );
    }
    if let Some(system_prompt) = system_prompt {
        meta.insert("systemPrompt".into(), system_prompt.clone());
    }
    Some(meta)
}

/// Negotiate one prompt-free ACP Session and retain the full advertised
/// capability descriptors. The caller bounds and reaps the process.
pub async fn negotiate_capabilities(
    channel: &mut dyn AgentChannel,
    config: &AcpCapabilityProbeConfig,
) -> Result<NegotiatedAcpCapabilities, AcpError> {
    let cwd = config.session_cwd.as_deref().unwrap_or("/");
    let resolver = RejectPermission;
    let mut sink = crate::DiscardRunFacts;
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
    let session = open_new_session(
        &mut wire,
        &mut sink,
        &mut seq,
        &resolver,
        NewSessionConfig {
            cwd,
            mcp_servers: &[],
            mcp_execution_context: None,
            meta: None,
        },
    )
    .await?;
    Ok(crate::capabilities::project(init, session))
}

// Handshake deadlines share the setup module. The full driver tests retain
// initialize/session-new timeout coverage at their actual call boundary.
pub(super) async fn handshake_step<T>(
    deadline: std::time::Duration,
    stage: &'static str,
    operation: impl std::future::Future<Output = Result<T, AcpError>>,
) -> Result<T, AcpError> {
    tokio::time::timeout(deadline, operation)
        .await
        .map_err(|_| {
            AcpError::Io(format!(
                "ACP {stage} handshake timed out after {} ms",
                deadline.as_millis()
            ))
        })?
}

/// Validate the pinned capability and apply the exact Session configuration.
/// The public driver tests cover unsupported selections and their wire effects.
pub(super) async fn configure_session(
    wire: &mut Wire<'_>,
    sink: &mut dyn RunFactAppender,
    seq: &mut u64,
    config: &mut crate::TurnConfig<'_>,
    session_id: &agent_client_protocol::SessionId,
    negotiated: &NegotiatedAcpCapabilities,
    progress: HandshakeProgress,
) -> Result<PromptReady, AcpError> {
    use super::{ID_SET_CONFIG_OPTION, ID_SET_MODE, ID_SET_MODEL};
    use agent_client_protocol::{
        SessionModeId, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
        SetSessionModeRequest, SetSessionModelRequest, SetSessionModelResponse,
    };
    let deadline = config.handshake_step_deadline;
    let resolver = config.resolver;
    if let Some(expected) = &config.expected_capability {
        let actual = awaken_acp_contract::capability_fingerprint(
            &expected.adapter_id,
            &expected.adapter_version,
            negotiated,
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
        handshake_step(deadline, "session/set_mode", async {
            wire.send_request(
                ID_SET_MODE,
                AGENT_METHOD_NAMES.session_set_mode,
                SetSessionModeRequest::new(session_id.clone(), SessionModeId::new(mode.as_str())),
            )
            .await?;
            pump_to_response(wire, ID_SET_MODE, sink, seq, resolver).await?;
            Ok(())
        })
        .await?;
    }

    // 2c. session/set_model — some provider-agnostic adapters own model
    // selection at the protocol layer rather than reading an environment key.
    // Fail closed unless this exact Session advertised the model interface.
    if let Some(model) = config.session_model.clone() {
        if !progress.supports_session_model {
            return Err(AcpError::Frame(
                "configured ACP session model selection was not advertised".into(),
            ));
        }
        handshake_step(deadline, "session/set_model", async {
            wire.send_request(
                ID_SET_MODEL,
                AGENT_METHOD_NAMES.session_set_model,
                SetSessionModelRequest::new(session_id.clone(), model),
            )
            .await?;
            let _: SetSessionModelResponse =
                parse(pump_to_response(wire, ID_SET_MODEL, sink, seq, resolver).await?)?;
            Ok(())
        })
        .await?;
    }

    // 2d. session/set_config_option — exact backend-owned model selection. The
    // option must be advertised by this exact Session and the agent must accept
    // the value; either failure is terminal and never falls back to its default.
    for (index, selection) in config
        .session_config_options
        .clone()
        .into_iter()
        .enumerate()
    {
        if !available_config_options.contains(&selection.config_id) {
            return Err(AcpError::Frame(format!(
                "configured ACP session option `{}` was not advertised",
                selection.config_id
            )));
        }
        handshake_step(deadline, "session/set_config_option", async {
            wire.send_request(
                ID_SET_CONFIG_OPTION + index as u64,
                AGENT_METHOD_NAMES.session_set_config_option,
                SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    selection.config_id,
                    selection.value,
                ),
            )
            .await?;
            let _: SetSessionConfigOptionResponse = parse(
                pump_to_response(
                    wire,
                    ID_SET_CONFIG_OPTION + index as u64,
                    sink,
                    seq,
                    resolver,
                )
                .await?,
            )?;
            Ok(())
        })
        .await?;
    }

    progress.configured()?.into_prompt_ready()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{AgentIo, RecordingResolver, RecordingSink, channel};
    use super::super::{AcpProjectedEvent, run_turn_with_config};
    #[tokio::test]
    async fn alias_metadata_survives_new_load_and_missing_session_recovery() {
        // Cause graph: aliases x prior Session x advertised load x missing id
        // + max-turn budget -> new/load/recovery request -> identical constrained
        // SDK metadata -> prompt.
        // R1 absent aliases => no extension; R2 new; R3 load; R4 load missing => new;
        // R5 load unavailable => new. S1 absent/string/append systemPrompt crosses
        // every R1-R5 branch identically, appears only at top level, and never
        // changes aliases, tools or the permission identity.
        // Metadata carries no credential or arbitrary options.
        // A1 declared alias + explicit pinned adapter toolName -> one identical
        // identity in projected facts and permission; A2 absent alias or A3
        // undeclared toolName -> retain the raw title. These rules apply on new,
        // load and missing-session recovery, with complete permission arguments.
        for (enabled, prior, can_load, missing, reported_name) in [
            (false, false, false, false, "Bash"),
            (true, false, false, false, "Bash"),
            (true, true, true, false, "Bash"),
            (true, true, true, true, "Bash"),
            (true, true, false, false, "Bash"),
            (true, false, false, false, "Undeclared"),
        ] {
            for system_prompt in [
                None,
                Some(serde_json::json!("冻结规则")),
                Some(serde_json::json!({"append":"追加规则"})),
            ] {
                let expected_system_prompt = system_prompt.clone();
                let (mut ours, theirs) = channel();
                let agent = tokio::spawn(async move {
                    let mut io = AgentIo::new(theirs);
                    io.read().await;
                    io.write_line(&serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":can_load}}}).to_string()).await;
                    let attempts = if prior && can_load && missing { 2 } else { 1 };
                    for attempt in 0..attempts {
                        let request: serde_json::Value = io.read().await.unwrap();
                        let load = prior && can_load && attempt == 0;
                        assert_eq!(
                            request["method"],
                            if load { "session/load" } else { "session/new" }
                        );
                        let meta = &request["params"]["_meta"];
                        if enabled {
                            assert_eq!(
                                meta["claudeCode"]["options"],
                                serde_json::json!({"toolAliases":{"Bash":"mcp__awaken_session__bash"},"tools":[],"disallowedTools":["Bash"],"maxTurns":8})
                            );
                        } else {
                            assert!(meta.get("claudeCode").is_none());
                            assert_eq!(meta.is_null(), expected_system_prompt.is_none());
                        }
                        assert_eq!(
                            meta.get("systemPrompt"),
                            expected_system_prompt.as_ref(),
                            "S1"
                        );
                        assert!(
                            meta["claudeCode"]["options"].get("systemPrompt").is_none(),
                            "one prompt owner"
                        );
                        let reply = if load && missing {
                            serde_json::json!({"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"Session not found"}})
                        } else {
                            serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1"}})
                        };
                        io.write_line(&reply.to_string()).await;
                    }
                    let prompt: serde_json::Value = io.read().await.unwrap();
                    assert_eq!(prompt["method"], "session/prompt");
                    io.write_line(&serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":"sess-1","update":{"sessionUpdate":"tool_call","toolCallId":"shell-1",
                    "title":"Terminal","kind":"execute","rawInput":{},"_meta":{"claudeCode":{"toolName":reported_name}}}
                }}).to_string()).await;
                    io.write_line(&serde_json::json!({"jsonrpc":"2.0","id":42,"method":"session/request_permission","params":{
                    "sessionId":"sess-1","toolCall":{"toolCallId":"shell-1","title":"Terminal","kind":"execute","rawInput":{"command":"pwd"}},
                    "options":[{"optionId":"allow","kind":"allow_once","name":"Allow"}]
                }}).to_string()).await;
                    let permission: serde_json::Value = io.read().await.unwrap();
                    assert_eq!(permission["result"]["outcome"]["optionId"], "allow");
                    io.write_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
                        .await;
                });
                let mut sink = RecordingSink::default();
                let resolver = RecordingResolver::default();
                let mut config = crate::TurnConfig::new(&resolver);
                config.system_prompt = system_prompt;
                if enabled {
                    config
                        .tool_aliases
                        .insert("Bash".into(), "mcp__awaken_session__bash".into());
                    config.max_turn_requests = Some(8);
                }
                if prior {
                    config.session_id = Some("sess-1".into());
                }
                run_turn_with_config(ours.as_mut(), "task", &mut sink, &mut config, None)
                    .await
                    .unwrap();
                agent.await.unwrap();
                let expected = if enabled && reported_name == "Bash" {
                    "mcp__awaken_session__bash"
                } else {
                    "Terminal"
                };
                let seen = resolver.seen.lock().unwrap();
                assert_eq!(seen.len(), 1);
                assert_eq!(seen[0].tool, expected, "A1/A2/A3 exact identity");
                assert_eq!(seen[0].arguments, serde_json::json!({"command":"pwd"}));
                assert!(
                    sink.events
                        .iter()
                        .filter_map(|(_, event)| {
                            if let AcpProjectedEvent::ToolCall { name, .. } = event {
                                Some(name.as_str())
                            } else {
                                None
                            }
                        })
                        .all(|name| name == expected),
                    "projection and permission share one identity"
                );
            }
        }
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    const EVERY_PHASE: [HandshakePhase; 4] = [
        HandshakePhase::Started,
        HandshakePhase::Initialized,
        HandshakePhase::SessionOpened,
        HandshakePhase::Configured,
    ];

    const fn listed_phase(phase: HandshakePhase) -> usize {
        match phase {
            HandshakePhase::Started => 0,
            HandshakePhase::Initialized => 1,
            HandshakePhase::SessionOpened => 2,
            HandshakePhase::Configured => 3,
        }
    }

    const EVERY_STEP: [HandshakeStep; 3] = [
        HandshakeStep::Initialize,
        HandshakeStep::OpenSession,
        HandshakeStep::Configure,
    ];

    const fn listed_step(step: HandshakeStep) -> usize {
        match step {
            HandshakeStep::Initialize => 0,
            HandshakeStep::OpenSession => 1,
            HandshakeStep::Configure => 2,
        }
    }

    #[test]
    fn every_handshake_phase_and_step_is_listed_once() {
        for (index, phase) in EVERY_PHASE.into_iter().enumerate() {
            assert_eq!(listed_phase(phase), index);
        }
        for (index, step) in EVERY_STEP.into_iter().enumerate() {
            assert_eq!(listed_step(step), index);
        }
    }

    /// @exhaustive HandshakePhase(4) x HandshakeStep(3) = 12
    /// @obligation acp.prompt_is_never_sent_before_bounded_handshake_completion
    #[test]
    fn acp_prompt_admission_requires_exact_completed_handshake() {
        // Cause/effect decision table over the production transition kernel:
        // | phase          | completed step | effect                         |
        // | started        | initialize     | initialized                    |
        // | initialized    | open session   | session opened                 |
        // | session opened | configure      | configured                     |
        // | every other pair               | reject without phase advance   |
        // | configured     | prompt admit   | admission predicate is true    |
        // | earlier phase  | prompt admit   | admission predicate is false   |
        // A failed or timed-out async step never calls this completion edge, so
        // it cannot authorize the later production prompt request.
        for phase in EVERY_PHASE {
            for step in EVERY_STEP {
                let expected = match (phase, step) {
                    (HandshakePhase::Started, HandshakeStep::Initialize) => {
                        Some(HandshakePhase::Initialized)
                    }
                    (HandshakePhase::Initialized, HandshakeStep::OpenSession) => {
                        Some(HandshakePhase::SessionOpened)
                    }
                    (HandshakePhase::SessionOpened, HandshakeStep::Configure) => {
                        Some(HandshakePhase::Configured)
                    }
                    _ => None,
                };
                assert_eq!(advance_handshake(phase, step), expected);
                assert_eq!(
                    prompt_is_admitted(phase),
                    phase == HandshakePhase::Configured
                );
            }
        }
    }
}

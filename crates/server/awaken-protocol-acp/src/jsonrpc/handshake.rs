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

#[cfg(kani)]
/// @obligation acp.prompt_is_never_sent_before_bounded_handshake_completion
/// @kani features=real-acp
#[kani::proof]
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
    let phase = match (kani::any::<bool>(), kani::any::<bool>()) {
        (false, false) => HandshakePhase::Started,
        (false, true) => HandshakePhase::Initialized,
        (true, false) => HandshakePhase::SessionOpened,
        (true, true) => HandshakePhase::Configured,
    };
    let step = match (kani::any::<bool>(), kani::any::<bool>()) {
        (false, false) => HandshakeStep::Initialize,
        (false, true) => HandshakeStep::OpenSession,
        (true, false) | (true, true) => HandshakeStep::Configure,
    };
    let expected = match (phase, step) {
        (HandshakePhase::Started, HandshakeStep::Initialize) => Some(HandshakePhase::Initialized),
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
            .mcp_servers(to_acp_mcp_servers(config.mcp_servers))
            .meta(config.meta),
    )
    .await?;
    parse(pump_to_response(wire, ID_NEW_SESSION, sink, seq, resolver).await?)
}

/// Pinned Claude adapter extension. Disable builtin objects as well as redirecting
/// model-emitted names; this does not grant authority or convert tool arguments.
pub(super) fn claude_session_meta(
    aliases: &std::collections::BTreeMap<String, String>,
    max_turns: Option<usize>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if aliases.is_empty() && max_turns.is_none() {
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
    Some(
        serde_json::json!({"claudeCode": {"options": options}})
            .as_object()
            .expect("metadata object")
            .clone(),
    )
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

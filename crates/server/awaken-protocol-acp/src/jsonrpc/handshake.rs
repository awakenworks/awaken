//! ACP initialize/authenticate/session setup shared by turns and capability probes.

use agent_client_protocol::{
    AGENT_METHOD_NAMES, AuthenticateRequest, AuthenticateResponse, ClientCapabilities,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, ProtocolVersion,
};
use awaken_acp_contract::{AcpCapabilityProbeConfig, NegotiatedAcpCapabilities};
use awaken_agent_channel::AgentChannel;

use super::{
    AcpError, ID_AUTHENTICATE, ID_INITIALIZE, ID_NEW_SESSION, PermissionResolver, RejectPermission,
    RunFactAppender, Wire, parse, pump_to_response, to_acp_mcp_servers,
};

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

pub(super) async fn open_new_session(
    wire: &mut Wire<'_>,
    sink: &mut dyn RunFactAppender,
    seq: &mut u64,
    resolver: &dyn PermissionResolver,
    cwd: &str,
    mcp_servers: &[crate::SessionMcpServer],
    aliases: &std::collections::BTreeMap<String, String>,
) -> Result<NewSessionResponse, AcpError> {
    wire.send_request(
        ID_NEW_SESSION,
        AGENT_METHOD_NAMES.session_new,
        NewSessionRequest::new(cwd)
            .mcp_servers(to_acp_mcp_servers(mcp_servers))
            .meta(tool_alias_meta(aliases)),
    )
    .await?;
    parse(pump_to_response(wire, ID_NEW_SESSION, sink, seq, resolver).await?)
}

/// Pinned Claude adapter extension. Disable builtin objects as well as redirecting
/// model-emitted names; this does not grant authority or convert tool arguments.
pub(super) fn tool_alias_meta(
    aliases: &std::collections::BTreeMap<String, String>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if aliases.is_empty() {
        return None;
    }
    Some(serde_json::json!({"claudeCode": {"options": {
        "toolAliases": aliases, "tools": [], "disallowedTools": aliases.keys().collect::<Vec<_>>()
    }}}).as_object().expect("metadata object").clone())
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
        cwd,
        &[],
        &Default::default(),
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
    supports_session_model: bool,
) -> Result<(), AcpError> {
    use super::{ID_SET_CONFIG_OPTION, ID_SET_MODE, ID_SET_MODEL};
    use agent_client_protocol::{
        SessionModeId, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
        SetSessionModeRequest, SetSessionModelRequest, SetSessionModelResponse,
    };
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
        wire.send_request(
            ID_SET_MODE,
            AGENT_METHOD_NAMES.session_set_mode,
            SetSessionModeRequest::new(session_id.clone(), SessionModeId::new(mode.as_str())),
        )
        .await?;
        pump_to_response(wire, ID_SET_MODE, sink, seq, resolver).await?;
    }

    // 2c. session/set_model — some provider-agnostic adapters own model
    // selection at the protocol layer rather than reading an environment key.
    // Fail closed unless this exact Session advertised the model interface.
    if let Some(model) = config.session_model.clone() {
        if !supports_session_model {
            return Err(AcpError::Frame(
                "configured ACP session model selection was not advertised".into(),
            ));
        }
        wire.send_request(
            ID_SET_MODEL,
            AGENT_METHOD_NAMES.session_set_model,
            SetSessionModelRequest::new(session_id.clone(), model),
        )
        .await?;
        let _: SetSessionModelResponse =
            parse(pump_to_response(wire, ID_SET_MODEL, sink, seq, resolver).await?)?;
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
    }

    Ok(())
}

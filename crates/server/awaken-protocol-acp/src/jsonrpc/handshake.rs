//! ACP initialize/authenticate/session setup shared by turns and capability probes.

use agent_client_protocol::{
    AGENT_METHOD_NAMES, AuthenticateRequest, AuthenticateResponse, ClientCapabilities,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, ProtocolVersion,
};
use awaken_acp_contract::{AcpCapabilityProbeConfig, NegotiatedAcpCapabilities};
use awaken_agent_channel::AgentChannel;

use super::{
    AcpError, ID_AUTHENTICATE, ID_INITIALIZE, ID_NEW_SESSION, PermissionAsk, PermissionResolver,
    PermissionVerdict, RunFactAppender, Wire, parse, pump_to_response, to_acp_mcp_servers,
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
    let session = open_new_session(&mut wire, &mut sink, &mut seq, &resolver, cwd, &[]).await?;
    Ok(crate::capabilities::project(init, session))
}

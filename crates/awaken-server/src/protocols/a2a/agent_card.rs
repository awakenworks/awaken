use std::collections::BTreeMap;

use awaken_protocol_a2a::{
    AgentCapabilities, AgentCard, AgentInterface, AgentProvider, AgentSkill,
};
use axum::Json;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::app::ProtocolRoutesState;

use super::common::{
    ensure_runnable_agent, ensure_supported_version, forwarded_header, public_agent_id,
};
use super::error::A2aError;
use super::types::{
    A2A_VERSION, EXTENDED_CARD_SECURITY_SCHEME_ID, INTERFACE_BASE_PATH, SUPPORTED_OUTPUT_MODE,
};

pub(super) async fn a2a_agent_card(
    st: ProtocolRoutesState,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Result<Json<AgentCard>, A2aError> {
    super::common::ensure_supported_version_from_request(&headers, &uri)?;
    let agent_id = public_agent_id(&st)?;
    Ok(Json(build_agent_card(
        &st, &headers, &agent_id, None, false,
    )))
}

pub(super) async fn a2a_extended_agent_card_default(
    st: ProtocolRoutesState,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    if !supports_extended_agent_card(&st) {
        return Err(A2aError::unsupported_operation(
            "extendedAgentCard is not configured for this agent",
        ));
    }
    ensure_extended_card_auth(&st, &headers)?;
    let agent_id = public_agent_id(&st)?;
    Ok(Json(build_agent_card(&st, &headers, &agent_id, None, true)).into_response())
}

pub(super) async fn a2a_extended_agent_card_tenant(
    st: ProtocolRoutesState,
    tenant: String,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    if !supports_extended_agent_card(&st) {
        return Err(A2aError::unsupported_operation(
            "extendedAgentCard is not configured for this agent",
        ));
    }
    ensure_runnable_agent(&st, &tenant)?;
    ensure_extended_card_auth(&st, &headers)?;
    Ok(Json(build_agent_card(
        &st,
        &headers,
        &tenant,
        Some(&tenant),
        true,
    ))
    .into_response())
}

fn build_agent_card(
    st: &ProtocolRoutesState,
    headers: &HeaderMap,
    agent_id: &str,
    tenant: Option<&str>,
    _extended: bool,
) -> AgentCard {
    let supports_extended_card = supports_extended_agent_card(st);
    let security_schemes = if supports_extended_card {
        BTreeMap::from([(
            EXTENDED_CARD_SECURITY_SCHEME_ID.to_string(),
            json!({
                "httpAuthSecurityScheme": {
                    "scheme": "Bearer"
                }
            }),
        )])
    } else {
        BTreeMap::new()
    };
    let security = if supports_extended_card {
        vec![BTreeMap::from([(
            EXTENDED_CARD_SECURITY_SCHEME_ID.to_string(),
            Vec::new(),
        )])]
    } else {
        Vec::new()
    };

    AgentCard {
        name: agent_id.to_string(),
        description: format!("Awaken AI agent '{agent_id}'"),
        supported_interfaces: vec![AgentInterface {
            url: interface_url(headers, tenant),
            protocol_binding: "HTTP+JSON".to_string(),
            protocol_version: A2A_VERSION.to_string(),
            tenant: tenant.map(ToOwned::to_owned),
        }],
        provider: Some(AgentProvider {
            organization: "Awaken".to_string(),
            url: Some(origin_url(headers)),
        }),
        version: env!("CARGO_PKG_VERSION").to_string(),
        documentation_url: None,
        capabilities: AgentCapabilities {
            streaming: true,
            push_notifications: true,
            state_transition_history: false,
            extended_agent_card: supports_extended_card,
        },
        security_schemes,
        security,
        default_input_modes: vec!["text/plain".to_string(), "application/json".to_string()],
        default_output_modes: vec![SUPPORTED_OUTPUT_MODE.to_string()],
        skills: vec![AgentSkill {
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: Some(format!("Interact with the '{agent_id}' Awaken agent.")),
            tags: vec!["awaken".to_string(), "agent".to_string()],
            examples: Vec::new(),
            input_modes: vec!["text/plain".to_string(), "application/json".to_string()],
            output_modes: vec![SUPPORTED_OUTPUT_MODE.to_string()],
        }],
        signatures: Vec::new(),
        icon_url: None,
    }
}

pub(super) fn supports_extended_agent_card(st: &ProtocolRoutesState) -> bool {
    st.a2a_extended_card_bearer_token.is_some()
}

fn ensure_extended_card_auth(
    st: &ProtocolRoutesState,
    headers: &HeaderMap,
) -> Result<(), A2aError> {
    let Some(expected) = st.a2a_extended_card_bearer_token.as_ref() else {
        return Err(A2aError::unsupported_operation(
            "extendedAgentCard is not configured for this agent",
        ));
    };
    let Some(auth) = forwarded_header(headers, "authorization") else {
        return Err(A2aError::unauthenticated(
            "missing Authorization header for extendedAgentCard",
        ));
    };
    let Some(token) = crate::auth::strip_bearer_prefix(auth) else {
        return Err(A2aError::unauthenticated(
            "Authorization header must use Bearer authentication",
        ));
    };
    if token.trim() != expected.expose_secret() {
        return Err(A2aError::unauthenticated(
            "invalid bearer token for extendedAgentCard",
        ));
    }
    Ok(())
}

fn origin_url(headers: &HeaderMap) -> String {
    let scheme = forwarded_header(headers, "x-forwarded-proto").unwrap_or("http");
    let host = forwarded_header(headers, "x-forwarded-host")
        .or_else(|| forwarded_header(headers, "host"))
        .unwrap_or("localhost");
    format!("{scheme}://{host}")
}

fn interface_url(headers: &HeaderMap, tenant: Option<&str>) -> String {
    let base = origin_url(headers);
    match tenant {
        Some(tenant) => format!("{base}{INTERFACE_BASE_PATH}/{tenant}"),
        None => format!("{base}{INTERFACE_BASE_PATH}"),
    }
}

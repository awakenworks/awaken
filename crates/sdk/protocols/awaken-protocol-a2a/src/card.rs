//! Agent-card discovery and authenticated extended-card projections.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::router::{JSONRPC_PATH, a2a_fault, v1_json_response};
use crate::state::A2aState;
use crate::types::{AgentCapabilities, AgentCard, AgentInterface, AgentSkill};
use crate::v1::agent_card_value as v1_agent_card_value;
use crate::version::{A2A_VERSION_HEADER, ProtocolVersion, negotiate_version};

pub(crate) async fn card(State(rt): State<A2aState>, headers: HeaderMap) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let version = match negotiate_version(&headers) {
        Ok(version) => version,
        Err(message) => return a2a_fault(StatusCode::BAD_REQUEST, -32009, message),
    };
    let origin = format!("http://{host}");
    let value = match version {
        ProtocolVersion::V03 => {
            serde_json::to_value(card_at(&rt, &origin)).expect("agent card serializes")
        }
        ProtocolVersion::V1 => v1_agent_card_value(&rt.runtime.model(), &origin),
    };
    let mut response = Json(value).into_response();
    response.headers_mut().insert(
        header::VARY,
        axum::http::HeaderValue::from_static(A2A_VERSION_HEADER),
    );
    response
}

pub(crate) async fn extended_card_rest(State(rt): State<A2aState>, headers: HeaderMap) -> Response {
    extended_card_response(&rt, &headers)
}

pub(crate) async fn extended_card_tenant_rest(
    State(rt): State<A2aState>,
    Path(_tenant): Path<String>,
    headers: HeaderMap,
) -> Response {
    extended_card_response(&rt, &headers)
}

fn extended_card_response(rt: &A2aState, headers: &HeaderMap) -> Response {
    let version = match negotiate_version(headers) {
        Ok(version) => version,
        Err(message) => return a2a_fault(StatusCode::BAD_REQUEST, -32009, message),
    };
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let origin = format!("http://{host}");
    match version {
        ProtocolVersion::V1 => v1_json_response(
            StatusCode::OK,
            v1_agent_card_value(&rt.runtime.model(), &origin),
        ),
        ProtocolVersion::V03 => Json(card_at(rt, &origin)).into_response(),
    }
}

fn card_at(rt: &A2aState, origin: &str) -> AgentCard {
    let mut card = agent_card(&rt.runtime.model());
    card.url = format!("{origin}{JSONRPC_PATH}");
    card.additional_interfaces = vec![AgentInterface {
        url: origin.to_string(),
        transport: "HTTP+JSON".into(),
    }];
    card
}

/// The public discovery card. Request-specific absolute URLs are added by the
/// handlers above.
pub fn agent_card(model: &str) -> AgentCard {
    AgentCard {
        name: "assistant".to_string(),
        description: format!("Awaken agent over model `{model}`"),
        documentation_url: None,
        icon_url: None,
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: "0.3.0".to_string(),
        provider: None,
        url: String::new(),
        preferred_transport: Some("JSONRPC".to_string()),
        additional_interfaces: Vec::new(),
        capabilities: AgentCapabilities {
            streaming: true,
            push_notifications: true,
            extensions: Vec::new(),
            state_transition_history: None,
        },
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string()],
        skills: vec![AgentSkill {
            id: "chat".to_string(),
            name: "Chat".to_string(),
            description: "General conversational assistance".to_string(),
            tags: vec!["chat".to_string()],
            examples: Vec::new(),
            input_modes: Vec::new(),
            output_modes: Vec::new(),
            security: Vec::new(),
        }],
        security_schemes: Default::default(),
        security: Vec::new(),
        signatures: Vec::new(),
        supports_authenticated_extended_card: Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_card_names_the_model_and_pins_the_protocol() {
        let card = agent_card("echo-model");
        assert_eq!(card.protocol_version, "0.3.0");
        assert!(card.capabilities.streaming);
        assert!(card.capabilities.push_notifications);
        assert!(card.description.contains("echo-model"));
    }
}

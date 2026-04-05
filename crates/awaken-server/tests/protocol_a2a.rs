//! A2A v1.0 type contract tests on the server re-export surface.

use awaken_server::protocols::a2a::http::{
    A2aMessage, AgentCapabilities, AgentCard, AgentInterface, AgentSkill, MessageRole, Part,
    SendMessageConfiguration, SendMessageRequest, TaskState,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[test]
fn agent_card_roundtrip_uses_supported_interfaces() {
    let card = AgentCard {
        name: "test-agent".into(),
        description: "A test agent".into(),
        supported_interfaces: vec![AgentInterface {
            url: "http://localhost:3000/v1/a2a".into(),
            protocol_binding: "HTTP+JSON".into(),
            protocol_version: "1.0".into(),
            tenant: Some("alpha".into()),
        }],
        provider: None,
        version: "1.0.0".into(),
        documentation_url: None,
        capabilities: AgentCapabilities {
            streaming: false,
            push_notifications: false,
            state_transition_history: false,
            extended_agent_card: false,
        },
        security_schemes: BTreeMap::new(),
        security: Vec::new(),
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![AgentSkill {
            id: "chat".into(),
            name: "Chat".into(),
            description: Some("Conversational assistance".into()),
            tags: vec!["chat".into()],
            examples: Vec::new(),
            input_modes: vec!["text/plain".into()],
            output_modes: vec!["text/plain".into()],
        }],
        signatures: Vec::new(),
        icon_url: None,
    };

    let value = serde_json::to_value(&card).unwrap();
    assert!(value.get("supportedInterfaces").is_some());
    assert!(
        value.get("url").is_none(),
        "legacy top-level url must not be emitted"
    );
    assert!(
        value.get("id").is_none(),
        "legacy top-level id must not be emitted"
    );

    let parsed: AgentCard = serde_json::from_value(value).unwrap();
    assert_eq!(parsed.supported_interfaces[0].protocol_version, "1.0");
    assert_eq!(
        parsed.supported_interfaces[0].tenant.as_deref(),
        Some("alpha")
    );
}

#[test]
fn agent_capabilities_default_to_false() {
    let capabilities: AgentCapabilities = serde_json::from_value(json!({})).unwrap();
    assert!(!capabilities.streaming);
    assert!(!capabilities.push_notifications);
    assert!(!capabilities.state_transition_history);
    assert!(!capabilities.extended_agent_card);
}

#[test]
fn part_serializes_without_legacy_type_discriminator() {
    let part = Part::text("hello");
    assert_eq!(
        serde_json::to_value(part).unwrap(),
        json!({"text": "hello"})
    );
}

#[test]
fn send_message_request_uses_role_and_wrapper_parts() {
    let request = SendMessageRequest {
        tenant: Some("alpha".into()),
        message: A2aMessage {
            task_id: Some("task-1".into()),
            context_id: Some("task-1".into()),
            message_id: "msg-1".into(),
            role: MessageRole::User,
            parts: vec![Part::text("hello")],
            metadata: None,
        },
        configuration: Some(SendMessageConfiguration {
            accepted_output_modes: vec!["text/plain".into()],
            task_push_notification_config: Some(json!({"url": "https://example.com"})),
            history_length: Some(2),
            return_immediately: Some(true),
        }),
        metadata: Some(json!({"source": "test"})),
    };

    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(value["message"]["role"], "ROLE_USER");
    assert_eq!(value["message"]["parts"][0], json!({"text": "hello"}));
    assert_eq!(value["configuration"]["returnImmediately"], true);
    assert_eq!(value["configuration"]["historyLength"], 2);
}

#[test]
fn send_message_configuration_accepts_push_notification_alias() {
    let configuration: SendMessageConfiguration = serde_json::from_value(json!({
        "acceptedOutputModes": ["text/plain"],
        "pushNotificationConfig": {"url": "https://example.com"},
        "historyLength": 3,
        "returnImmediately": true
    }))
    .unwrap();

    assert_eq!(configuration.accepted_output_modes, vec!["text/plain"]);
    assert!(configuration.task_push_notification_config.is_some());
    assert_eq!(configuration.history_length, Some(3));
    assert_eq!(configuration.return_immediately, Some(true));
}

#[test]
fn task_state_serializes_to_spec_enum_names() {
    let value: Value = serde_json::to_value(TaskState::Completed).unwrap();
    assert_eq!(value, json!("TASK_STATE_COMPLETED"));

    let parsed: TaskState = serde_json::from_value(json!("TASK_STATE_CANCELED")).unwrap();
    assert_eq!(parsed, TaskState::Canceled);
}

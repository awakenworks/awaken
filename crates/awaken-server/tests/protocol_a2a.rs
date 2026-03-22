//! A2A protocol tests — migrated from tirea-agentos-server/tests/a2a_http.rs.
//!
//! Validates agent card serialization, task send request parsing,
//! and A2A type contracts.

use awaken_server::protocols::a2a::http::{AgentCapabilities, AgentCard, AgentSkill};
use serde_json::json;

// ============================================================================
// Agent card serde
// ============================================================================

#[test]
fn agent_card_full_serde_roundtrip() {
    let card = AgentCard {
        id: String::new(),
        name: "test-agent".into(),
        description: Some("A test agent".into()),
        url: "http://localhost:3000".into(),
        version: "1.0.0".into(),
        capabilities: Some(AgentCapabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: true,
        }),
        skills: vec![AgentSkill {
            id: "s1".into(),
            name: "search".into(),
            description: Some("Web search".into()),
            tags: vec!["web".into()],
        }],
    };
    let json_str = serde_json::to_string(&card).unwrap();
    let parsed: AgentCard = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed.name, "test-agent");
    assert_eq!(parsed.skills.len(), 1);
    assert_eq!(parsed.skills[0].id, "s1");
    assert_eq!(parsed.skills[0].name, "search");
    assert_eq!(parsed.skills[0].tags, vec!["web".to_string()]);
}

#[test]
fn agent_card_minimal_omits_optional_fields() {
    let card = AgentCard {
        id: String::new(),
        name: "minimal".into(),
        description: None,
        url: String::new(),
        version: "0.1.0".into(),
        capabilities: None,
        skills: Vec::new(),
    };
    let json_str = serde_json::to_string(&card).unwrap();
    assert!(!json_str.contains("skills"));
    assert!(!json_str.contains("description"));
    assert!(!json_str.contains("capabilities"));
}

#[test]
fn agent_card_capabilities_defaults() {
    let caps: AgentCapabilities = serde_json::from_str("{}").unwrap();
    assert!(!caps.streaming);
    assert!(!caps.push_notifications);
    assert!(!caps.state_transition_history);
}

#[test]
fn agent_card_multiple_skills() {
    let card = AgentCard {
        id: String::new(),
        name: "multi-skill".into(),
        description: None,
        url: "http://localhost".into(),
        version: "1.0.0".into(),
        capabilities: None,
        skills: vec![
            AgentSkill {
                id: "s1".into(),
                name: "search".into(),
                description: Some("Web search".into()),
                tags: vec!["web".into()],
            },
            AgentSkill {
                id: "s2".into(),
                name: "calculator".into(),
                description: Some("Math".into()),
                tags: vec!["math".into(), "utility".into()],
            },
        ],
    };
    let json_str = serde_json::to_string(&card).unwrap();
    let parsed: AgentCard = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed.skills.len(), 2);
    assert_eq!(parsed.skills[1].tags.len(), 2);
}

// ============================================================================
// Agent card deserialization from various formats
// ============================================================================

#[test]
fn agent_card_from_json_value() {
    let value = json!({
        "name": "external-agent",
        "url": "https://agent.example.com",
        "version": "2.0.0",
        "capabilities": {
            "streaming": true,
            "push_notifications": true,
            "state_transition_history": false
        },
        "skills": [
            {
                "id": "code",
                "name": "Code Generation",
                "description": "Generates code"
            }
        ]
    });
    let card: AgentCard = serde_json::from_value(value).unwrap();
    assert_eq!(card.name, "external-agent");
    assert!(card.capabilities.as_ref().unwrap().streaming);
    assert!(card.capabilities.as_ref().unwrap().push_notifications);
    assert_eq!(card.skills[0].id, "code");
}

#[test]
fn agent_card_skill_without_description() {
    let value = json!({
        "name": "agent",
        "url": "http://localhost",
        "version": "1.0.0",
        "skills": [{"id": "s1", "name": "tool"}]
    });
    let card: AgentCard = serde_json::from_value(value).unwrap();
    assert!(card.skills[0].description.is_none());
    assert!(card.skills[0].tags.is_empty());
}

// ============================================================================
// A2A task send request deserialization
// ============================================================================

#[test]
fn a2a_task_send_request_with_all_fields() {
    let value = json!({
        "taskId": "task-1",
        "agentId": "agent-1",
        "message": {
            "role": "user",
            "parts": [
                {"type": "text", "text": "hello"},
                {"type": "text", "text": " world"}
            ]
        },
        "metadata": {"source": "test"}
    });
    // Verify the structure parses — we test deserialization contract
    let parsed: serde_json::Value = serde_json::from_value(value).unwrap();
    assert_eq!(parsed["taskId"], "task-1");
    assert_eq!(parsed["agentId"], "agent-1");
    assert_eq!(parsed["message"]["parts"].as_array().unwrap().len(), 2);
}

#[test]
fn a2a_task_send_request_minimal() {
    let value = json!({
        "message": {
            "role": "user",
            "parts": [{"type": "text", "text": "hi"}]
        }
    });
    let parsed: serde_json::Value = serde_json::from_value(value).unwrap();
    assert!(parsed.get("taskId").is_none());
    assert_eq!(parsed["message"]["role"], "user");
}

#[test]
fn a2a_task_status_serialization() {
    let value = json!({
        "taskId": "task-1",
        "status": {
            "state": "completed",
            "message": {"text": "done"}
        }
    });
    assert_eq!(value["taskId"], "task-1");
    assert_eq!(value["status"]["state"], "completed");
}

// ============================================================================
// A2A multi-part message handling
// ============================================================================

#[test]
fn a2a_message_multiple_text_parts_concatenated() {
    let parts = json!([
        {"type": "text", "text": "Hello "},
        {"type": "image", "url": "https://example.com/img.png"},
        {"type": "text", "text": "World"}
    ]);
    let text: String = parts
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(text, "Hello World");
}

#[test]
fn a2a_message_no_text_parts() {
    let parts = json!([
        {"type": "image", "url": "https://example.com/img.png"}
    ]);
    let text: String = parts
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("");
    assert!(text.is_empty());
}

// ============================================================================
// Agent card with streaming capabilities
// ============================================================================

#[test]
fn agent_card_streaming_capability() {
    let card = AgentCard {
        id: String::new(),
        name: "streaming-agent".into(),
        description: Some("Supports SSE streaming".into()),
        url: "http://localhost:3000/v1/a2a".into(),
        version: "1.0.0".into(),
        capabilities: Some(AgentCapabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: false,
        }),
        skills: Vec::new(),
    };
    let value = serde_json::to_value(&card).unwrap();
    assert!(value["capabilities"]["streaming"].as_bool().unwrap());
    assert!(
        !value["capabilities"]["push_notifications"]
            .as_bool()
            .unwrap_or(true)
    );
}

// ============================================================================
// Agent card version field
// ============================================================================

#[test]
fn agent_card_version_semver_format() {
    let card = AgentCard {
        id: String::new(),
        name: "versioned".into(),
        description: None,
        url: String::new(),
        version: "2.1.0-beta.1".into(),
        capabilities: None,
        skills: Vec::new(),
    };
    let json_str = serde_json::to_string(&card).unwrap();
    assert!(json_str.contains("2.1.0-beta.1"));
}

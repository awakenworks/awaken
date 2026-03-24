#![cfg(feature = "nats")]

use awaken_server::transport::nats::{NatsRunMessage, NatsRunRequest, convert_nats_messages};

// ---------------------------------------------------------------------------
// Deserialization: various message formats
// ---------------------------------------------------------------------------

#[test]
fn nats_request_deserialize_and_convert() {
    let req: NatsRunRequest = serde_json::from_value(serde_json::json!({
        "thread_id": "t1",
        "agent_id": "a1",
        "messages": [{"role":"user","content":"hi"}]
    }))
    .unwrap();
    let msgs = convert_nats_messages(req.messages);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text(), "hi");
}

#[test]
fn nats_request_deserialize_all_roles() {
    let req: NatsRunRequest = serde_json::from_value(serde_json::json!({
        "messages": [
            {"role":"user","content":"u"},
            {"role":"assistant","content":"a"},
            {"role":"system","content":"s"}
        ]
    }))
    .unwrap();
    let msgs = convert_nats_messages(req.messages);
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].text(), "u");
    assert_eq!(msgs[1].text(), "a");
    assert_eq!(msgs[2].text(), "s");
}

#[test]
fn nats_request_deserialize_minimal_json() {
    // Empty object should work because all fields have defaults
    let req: NatsRunRequest = serde_json::from_str("{}").unwrap();
    assert!(req.thread_id.is_none());
    assert!(req.agent_id.is_none());
    assert!(req.messages.is_empty());
}

// ---------------------------------------------------------------------------
// Deserialization: missing and extra fields
// ---------------------------------------------------------------------------

#[test]
fn nats_request_missing_optional_fields() {
    let req: NatsRunRequest =
        serde_json::from_str(r#"{"messages":[{"role":"user","content":"hi"}]}"#).unwrap();
    assert!(req.thread_id.is_none());
    assert!(req.agent_id.is_none());
    assert_eq!(req.messages.len(), 1);
}

#[test]
fn nats_request_extra_fields_ignored() {
    let req: NatsRunRequest = serde_json::from_value(serde_json::json!({
        "thread_id": "t1",
        "agent_id": "a1",
        "messages": [],
        "unknown_field": 42,
        "another": "ignored"
    }))
    .unwrap();
    assert_eq!(req.thread_id.as_deref(), Some("t1"));
    assert_eq!(req.agent_id.as_deref(), Some("a1"));
}

#[test]
fn nats_request_malformed_json_returns_error() {
    let result = serde_json::from_str::<NatsRunRequest>("not json");
    assert!(result.is_err());
}

#[test]
fn nats_request_truncated_json_returns_error() {
    let result = serde_json::from_str::<NatsRunRequest>(r#"{"thread_id": "t1"#);
    assert!(result.is_err());
}

#[test]
fn nats_request_wrong_type_for_messages() {
    let result = serde_json::from_str::<NatsRunRequest>(r#"{"messages":"not_an_array"}"#);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Role filtering
// ---------------------------------------------------------------------------

#[test]
fn nats_unknown_role_filtered() {
    let msgs = convert_nats_messages(vec![NatsRunMessage {
        role: "x".into(),
        content: "c".into(),
    }]);
    assert!(msgs.is_empty());
}

#[test]
fn nats_multiple_unknown_roles_all_filtered() {
    let msgs = convert_nats_messages(vec![
        NatsRunMessage {
            role: "function".into(),
            content: "a".into(),
        },
        NatsRunMessage {
            role: "tool".into(),
            content: "b".into(),
        },
        NatsRunMessage {
            role: "admin".into(),
            content: "c".into(),
        },
    ]);
    assert!(msgs.is_empty());
}

#[test]
fn nats_mixed_known_unknown_roles() {
    let msgs = convert_nats_messages(vec![
        NatsRunMessage {
            role: "user".into(),
            content: "keep".into(),
        },
        NatsRunMessage {
            role: "bogus".into(),
            content: "drop".into(),
        },
        NatsRunMessage {
            role: "assistant".into(),
            content: "keep2".into(),
        },
    ]);
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].text(), "keep");
    assert_eq!(msgs[1].text(), "keep2");
}

// ---------------------------------------------------------------------------
// Serialization roundtrip
// ---------------------------------------------------------------------------

#[test]
fn nats_run_request_serde_roundtrip() {
    let req = NatsRunRequest {
        thread_id: Some("t-1".to_string()),
        agent_id: Some("agent-a".to_string()),
        messages: vec![NatsRunMessage {
            role: "user".into(),
            content: "hello".into(),
        }],
    };
    let json = serde_json::to_string(&req).unwrap();
    let decoded: NatsRunRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.thread_id.as_deref(), Some("t-1"));
    assert_eq!(decoded.agent_id.as_deref(), Some("agent-a"));
    assert_eq!(decoded.messages.len(), 1);
    assert_eq!(decoded.messages[0].role, "user");
    assert_eq!(decoded.messages[0].content, "hello");
}

#[test]
fn nats_run_request_roundtrip_no_optional_fields() {
    let req = NatsRunRequest {
        thread_id: None,
        agent_id: None,
        messages: vec![],
    };
    let json = serde_json::to_string(&req).unwrap();
    let decoded: NatsRunRequest = serde_json::from_str(&json).unwrap();
    assert!(decoded.thread_id.is_none());
    assert!(decoded.agent_id.is_none());
    assert!(decoded.messages.is_empty());
}

#[test]
fn nats_run_message_serde_roundtrip() {
    let msg = NatsRunMessage {
        role: "system".into(),
        content: "you are helpful".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: NatsRunMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.role, "system");
    assert_eq!(decoded.content, "you are helpful");
}

// ---------------------------------------------------------------------------
// Empty message content handling
// ---------------------------------------------------------------------------

#[test]
fn nats_empty_content_preserved() {
    let msgs = convert_nats_messages(vec![NatsRunMessage {
        role: "user".into(),
        content: String::new(),
    }]);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text(), "");
}

#[test]
fn nats_whitespace_only_content_preserved() {
    let msgs = convert_nats_messages(vec![NatsRunMessage {
        role: "user".into(),
        content: "   ".into(),
    }]);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text(), "   ");
}

#[test]
fn nats_message_default_content_is_empty() {
    // content has serde(default), so missing content = ""
    let msg: NatsRunMessage = serde_json::from_str(r#"{"role":"user"}"#).unwrap();
    assert_eq!(msg.content, "");
}

// ---------------------------------------------------------------------------
// Multiple messages in single request
// ---------------------------------------------------------------------------

#[test]
fn nats_multiple_messages_converted_in_order() {
    let msgs = convert_nats_messages(vec![
        NatsRunMessage {
            role: "system".into(),
            content: "sys prompt".into(),
        },
        NatsRunMessage {
            role: "user".into(),
            content: "first".into(),
        },
        NatsRunMessage {
            role: "assistant".into(),
            content: "response".into(),
        },
        NatsRunMessage {
            role: "user".into(),
            content: "second".into(),
        },
    ]);
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0].text(), "sys prompt");
    assert_eq!(msgs[1].text(), "first");
    assert_eq!(msgs[2].text(), "response");
    assert_eq!(msgs[3].text(), "second");
}

#[test]
fn nats_many_messages_all_converted() {
    let input: Vec<NatsRunMessage> = (0..100)
        .map(|i| NatsRunMessage {
            role: "user".into(),
            content: format!("msg-{i}"),
        })
        .collect();
    let msgs = convert_nats_messages(input);
    assert_eq!(msgs.len(), 100);
    for (i, m) in msgs.iter().enumerate() {
        assert_eq!(m.text(), format!("msg-{i}"));
    }
}

// ---------------------------------------------------------------------------
// Large payload handling
// ---------------------------------------------------------------------------

#[test]
fn nats_large_payload_deserialize() {
    let large_content = "x".repeat(1_000_000); // 1 MB
    let req: NatsRunRequest = serde_json::from_value(serde_json::json!({
        "thread_id": "big",
        "messages": [{"role":"user","content": large_content}]
    }))
    .unwrap();
    let msgs = convert_nats_messages(req.messages);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text().len(), 1_000_000);
}

#[test]
fn nats_large_payload_serde_roundtrip() {
    let large_content = "y".repeat(500_000);
    let req = NatsRunRequest {
        thread_id: Some("big-rt".into()),
        agent_id: None,
        messages: vec![NatsRunMessage {
            role: "user".into(),
            content: large_content.clone(),
        }],
    };
    let json = serde_json::to_vec(&req).unwrap();
    let decoded: NatsRunRequest = serde_json::from_slice(&json).unwrap();
    assert_eq!(decoded.messages[0].content, large_content);
}

// ---------------------------------------------------------------------------
// NatsRunRequest from bytes (simulating NATS message payload)
// ---------------------------------------------------------------------------

#[test]
fn nats_request_from_bytes_slice() {
    let payload = br#"{"thread_id":"t1","messages":[{"role":"user","content":"hi"}]}"#;
    let req: NatsRunRequest = serde_json::from_slice(payload).unwrap();
    assert_eq!(req.thread_id.as_deref(), Some("t1"));
    assert_eq!(req.messages.len(), 1);
}

#[test]
fn nats_request_from_empty_bytes_is_error() {
    let result = serde_json::from_slice::<NatsRunRequest>(b"");
    assert!(result.is_err());
}

#[test]
fn nats_request_from_binary_garbage_is_error() {
    let result = serde_json::from_slice::<NatsRunRequest>(&[0xFF, 0xFE, 0x00, 0x01]);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Unicode and special characters
// ---------------------------------------------------------------------------

#[test]
fn nats_unicode_content_preserved() {
    let content = "Hello \u{1F600} world \u{4F60}\u{597D}";
    let msgs = convert_nats_messages(vec![NatsRunMessage {
        role: "user".into(),
        content: content.into(),
    }]);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text(), content);
}

#[test]
fn nats_unicode_roundtrip_through_json() {
    let content = "\u{1F4A9} emoji and CJK \u{6D4B}\u{8BD5}";
    let req = NatsRunRequest {
        thread_id: Some("unicode-thread-\u{2603}".into()),
        agent_id: None,
        messages: vec![NatsRunMessage {
            role: "user".into(),
            content: content.into(),
        }],
    };
    let json = serde_json::to_vec(&req).unwrap();
    let decoded: NatsRunRequest = serde_json::from_slice(&json).unwrap();
    assert_eq!(
        decoded.thread_id.as_deref(),
        Some("unicode-thread-\u{2603}")
    );
    assert_eq!(decoded.messages[0].content, content);
}

// ---------------------------------------------------------------------------
// Newlines and embedded JSON in content
// ---------------------------------------------------------------------------

#[test]
fn nats_newlines_in_content() {
    let content = "line1\nline2\nline3";
    let msgs = convert_nats_messages(vec![NatsRunMessage {
        role: "user".into(),
        content: content.into(),
    }]);
    assert_eq!(msgs[0].text(), content);
}

#[test]
fn nats_embedded_json_in_content() {
    let content = r#"{"key":"value","nested":{"a":1}}"#;
    let req = NatsRunRequest {
        thread_id: None,
        agent_id: None,
        messages: vec![NatsRunMessage {
            role: "user".into(),
            content: content.into(),
        }],
    };
    let json = serde_json::to_string(&req).unwrap();
    let decoded: NatsRunRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.messages[0].content, content);
}

// ---------------------------------------------------------------------------
// Duplicate messages
// ---------------------------------------------------------------------------

#[test]
fn nats_duplicate_messages_all_kept() {
    let msgs = convert_nats_messages(vec![
        NatsRunMessage {
            role: "user".into(),
            content: "same".into(),
        },
        NatsRunMessage {
            role: "user".into(),
            content: "same".into(),
        },
    ]);
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].text(), "same");
    assert_eq!(msgs[1].text(), "same");
}

// ---------------------------------------------------------------------------
// Case sensitivity of roles
// ---------------------------------------------------------------------------

#[test]
fn nats_role_case_sensitive() {
    // "User" (capitalized) is not a known role
    let msgs = convert_nats_messages(vec![
        NatsRunMessage {
            role: "User".into(),
            content: "dropped".into(),
        },
        NatsRunMessage {
            role: "ASSISTANT".into(),
            content: "dropped".into(),
        },
        NatsRunMessage {
            role: "System".into(),
            content: "dropped".into(),
        },
    ]);
    assert!(msgs.is_empty());
}

use super::*;
use serde_json::json;

#[test]
fn test_user_message() {
    let msg = Message::user("Hello");
    assert_eq!(msg.role, Role::User);
    assert_eq!(msg.text(), "Hello");
    assert!(msg.id.is_some());
}

#[test]
fn test_user_with_multimodal_content() {
    let msg = Message::user_with_content(vec![
        ContentBlock::text("Look at this:"),
        ContentBlock::image_url("https://example.com/img.png"),
    ]);
    assert_eq!(msg.role, Role::User);
    assert_eq!(msg.content.len(), 2);
    assert_eq!(msg.text(), "Look at this:");
}

#[test]
fn test_all_constructors_generate_uuid_v7_id() {
    let msgs = vec![
        Message::system("sys"),
        Message::internal_system("internal"),
        Message::user("usr"),
        Message::assistant("asst"),
        Message::assistant_with_tool_calls("tc", vec![]),
        Message::tool("c1", "result"),
    ];
    for msg in &msgs {
        let id = msg.id.as_ref().expect("message should have an id");
        assert_eq!(id.len(), 36, "id should be UUID format: {id}");
        assert_eq!(&id[14..15], "7", "UUID version should be 7: {id}");
    }
    let ids: std::collections::HashSet<&str> =
        msgs.iter().map(|m| m.id.as_deref().unwrap()).collect();
    assert_eq!(ids.len(), msgs.len());
}

#[test]
fn test_assistant_with_tool_calls() {
    let calls = vec![ToolCall::new("call_1", "search", json!({"query": "rust"}))];
    let msg = Message::assistant_with_tool_calls("Let me search", calls);
    assert_eq!(msg.role, Role::Assistant);
    assert_eq!(msg.text(), "Let me search");
    assert!(msg.tool_calls.is_some());
    assert_eq!(msg.tool_calls.as_ref().unwrap().len(), 1);
}

#[test]
fn test_tool_message() {
    let msg = Message::tool("call_1", "Result: 42");
    assert_eq!(msg.role, Role::Tool);
    assert_eq!(msg.text(), "Result: 42");
    assert_eq!(msg.tool_call_id.as_deref(), Some("call_1"));
}

#[test]
fn test_message_serialization() {
    let msg = Message::user("test");
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"role\":\"user\""));
    assert!(!json.contains("tool_calls"));
    assert!(!json.contains("tool_call_id"));
    assert!(!json.contains("metadata"));
}

#[test]
fn test_message_with_metadata_serialization() {
    let msg = Message::user("test").with_metadata(MessageMetadata {
        run_id: Some("run-1".to_string()),
        step_index: Some(3),
    });
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"run_id\":\"run-1\""));
    assert!(json.contains("\"step_index\":3"));

    let parsed: Message = serde_json::from_str(&json).unwrap();
    let meta = parsed.metadata.unwrap();
    assert_eq!(meta.run_id.as_deref(), Some("run-1"));
    assert_eq!(meta.step_index, Some(3));
}

#[test]
fn test_tool_call_serialization() {
    let call = ToolCall::new("id_1", "calculator", json!({"expr": "2+2"}));
    let json = serde_json::to_string(&call).unwrap();
    let parsed: ToolCall = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.id, "id_1");
    assert_eq!(parsed.name, "calculator");
    assert_eq!(parsed.arguments["expr"], "2+2");
}

#[test]
fn test_with_id_overrides_auto_generated() {
    let msg = Message::user("hi").with_id("custom-id".to_string());
    assert_eq!(msg.id.as_deref(), Some("custom-id"));
}

#[test]
fn test_gen_message_id_is_public_and_uuid_v7() {
    let id = gen_message_id();
    assert_eq!(id.len(), 36);
    assert_eq!(&id[14..15], "7");
}

#[test]
fn test_system_message() {
    let msg = Message::system("You are helpful");
    assert_eq!(msg.role, Role::System);
    assert_eq!(msg.text(), "You are helpful");
    assert_eq!(msg.visibility, Visibility::All);
}

#[test]
fn test_internal_system_message() {
    let msg = Message::internal_system("hidden reminder");
    assert_eq!(msg.role, Role::System);
    assert_eq!(msg.text(), "hidden reminder");
    assert_eq!(msg.visibility, Visibility::Internal);
}

#[test]
fn test_internal_user_message() {
    let msg = Message::internal_user("hidden reminder");
    assert_eq!(msg.role, Role::User);
    assert_eq!(msg.text(), "hidden reminder");
    assert_eq!(msg.visibility, Visibility::Internal);
}

#[test]
fn test_assistant_with_empty_tool_calls_omits_field() {
    let msg = Message::assistant_with_tool_calls("No tools", vec![]);
    assert!(msg.tool_calls.is_none());
    assert_eq!(msg.text(), "No tools");
}

#[test]
fn test_tool_with_content_blocks() {
    let msg = Message::tool_with_content(
        "call_1",
        vec![ContentBlock::text("part 1"), ContentBlock::text("part 2")],
    );
    assert_eq!(msg.role, Role::Tool);
    assert_eq!(msg.tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(msg.content.len(), 2);
    assert_eq!(msg.text(), "part 1part 2");
}

#[test]
fn test_message_full_serde_roundtrip_with_tool_calls() {
    let calls = vec![
        ToolCall::new("call_1", "search", json!({"query": "rust"})),
        ToolCall::new("call_2", "fetch", json!({"url": "https://example.com"})),
    ];
    let msg = Message::assistant_with_tool_calls("Multi-tool call", calls);
    let json = serde_json::to_string(&msg).unwrap();
    let parsed: Message = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.role, Role::Assistant);
    assert_eq!(parsed.text(), "Multi-tool call");
    let tc = parsed.tool_calls.unwrap();
    assert_eq!(tc.len(), 2);
    assert_eq!(tc[0].id, "call_1");
    assert_eq!(tc[0].name, "search");
    assert_eq!(tc[1].id, "call_2");
    assert_eq!(tc[1].name, "fetch");
}

#[test]
fn test_tool_message_serde_roundtrip() {
    let msg = Message::tool("call_1", r#"{"result": "hello"}"#);
    let json = serde_json::to_string(&msg).unwrap();
    let parsed: Message = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.role, Role::Tool);
    assert_eq!(parsed.tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(parsed.text(), r#"{"result": "hello"}"#);
}

#[test]
fn test_visibility_serde_roundtrip() {
    for vis in [Visibility::All, Visibility::Internal] {
        let json = serde_json::to_string(&vis).unwrap();
        let parsed: Visibility = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, vis);
    }
}

#[test]
fn test_visibility_default_is_all() {
    assert_eq!(Visibility::default(), Visibility::All);
    assert!(Visibility::All.is_default());
    assert!(!Visibility::Internal.is_default());
}

#[test]
fn test_role_serde_roundtrip() {
    for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
        let json = serde_json::to_string(&role).unwrap();
        let parsed: Role = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, role);
    }
}

#[test]
fn test_internal_message_omits_visibility_default() {
    let msg = Message::user("visible");
    let json = serde_json::to_string(&msg).unwrap();
    // Default visibility (All) should be omitted
    assert!(!json.contains("visibility"));

    let internal = Message::internal_system("hidden");
    let json = serde_json::to_string(&internal).unwrap();
    assert!(json.contains("\"visibility\":\"internal\""));
}

#[test]
fn test_message_metadata_default_omits_empty() {
    let meta = MessageMetadata::default();
    let json = serde_json::to_string(&meta).unwrap();
    assert!(!json.contains("run_id"));
    assert!(!json.contains("step_index"));
}

// ── Backward compatibility tests (migrated from uncarve) ──

#[test]
fn test_message_without_metadata_deserializes() {
    let json = r#"{"role":"user","content":[{"type":"text","text":"hello"}]}"#;
    let msg: Message = serde_json::from_str(json).unwrap();
    assert_eq!(msg.role, Role::User);
    assert!(msg.metadata.is_none());
    assert!(msg.id.is_none());
    assert_eq!(msg.visibility, Visibility::All);
}

// ── Role serialization value tests ──

#[test]
fn test_role_serialization_values() {
    assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
    assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
    assert_eq!(
        serde_json::to_string(&Role::Assistant).unwrap(),
        "\"assistant\""
    );
    assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"tool\"");
}

// ── Visibility serialization value tests ──

#[test]
fn test_visibility_serialization_values() {
    assert_eq!(serde_json::to_string(&Visibility::All).unwrap(), "\"all\"");
    assert_eq!(
        serde_json::to_string(&Visibility::Internal).unwrap(),
        "\"internal\""
    );
}

// ── Message clone and debug tests ──

#[test]
fn test_message_clone() {
    let msg = Message::user("hello");
    let cloned = msg.clone();
    assert_eq!(cloned.role, Role::User);
    assert_eq!(cloned.text(), "hello");
    assert_eq!(cloned.id, msg.id);
}

#[test]
fn test_message_debug() {
    let msg = Message::user("hello");
    let debug = format!("{:?}", msg);
    assert!(debug.contains("Message"));
    assert!(debug.contains("User"));
}

// ── ToolCall tests ──

#[test]
fn test_tool_call_clone() {
    let call = ToolCall::new("id_1", "search", json!({"q": "rust"}));
    let cloned = call.clone();
    assert_eq!(cloned.id, "id_1");
    assert_eq!(cloned.name, "search");
    assert_eq!(cloned.arguments, json!({"q": "rust"}));
}

#[test]
fn test_tool_call_debug() {
    let call = ToolCall::new("id_1", "search", json!({}));
    let debug = format!("{:?}", call);
    assert!(debug.contains("ToolCall"));
    assert!(debug.contains("search"));
}

// ── MessageMetadata tests ──

#[test]
fn test_message_metadata_serde_roundtrip() {
    let meta = MessageMetadata {
        run_id: Some("run-1".into()),
        step_index: Some(5),
    };
    let json = serde_json::to_string(&meta).unwrap();
    let parsed: MessageMetadata = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, meta);
}

#[test]
fn test_message_metadata_partial_fields() {
    let json = r#"{"run_id":"r1"}"#;
    let meta: MessageMetadata = serde_json::from_str(json).unwrap();
    assert_eq!(meta.run_id.as_deref(), Some("r1"));
    assert!(meta.step_index.is_none());
}

#[test]
fn message_record_projects_thread_sequence_and_producer() {
    let msg = Message::tool("call-1", "result")
        .with_id("msg-1".to_string())
        .with_metadata(MessageMetadata {
            run_id: Some("run-1".to_string()),
            step_index: Some(3),
        });

    let record = MessageRecord::from_message("thread-1", 7, msg);

    assert_eq!(record.message_id, "msg-1");
    assert_eq!(record.thread_id, "thread-1");
    assert_eq!(record.seq, 7);
    assert_eq!(record.produced_by_run_id.as_deref(), Some("run-1"));
    assert_eq!(record.step_index, Some(3));
    assert_eq!(record.tool_call_id.as_deref(), Some("call-1"));
}

#[test]
fn strip_unpaired_tool_calls_from_view_keeps_answered_calls_only() {
    let mut assistant = Message::assistant("tools");
    assistant.tool_calls = Some(vec![
        ToolCall::new("answered", "search", json!({})),
        ToolCall::new("orphaned", "dangerous", json!({})),
    ]);
    let mut messages = vec![
        Message::user("question"),
        assistant,
        Message::tool("answered", "result"),
    ];

    strip_unpaired_tool_calls_from_view(&mut messages);

    let calls = messages[1].tool_calls.as_ref().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "answered");
}

#[test]
fn strip_unpaired_tool_calls_from_view_honors_internal_retraction_marker() {
    let mut assistant = Message::assistant("tools");
    assistant.tool_calls = Some(vec![
        ToolCall::new("answered", "search", json!({})),
        ToolCall::new("retracted", "dangerous", json!({})),
    ]);
    let mut marker = Message::tool("retracted", "[tool call superseded]");
    marker.visibility = Visibility::Internal;
    let mut messages = vec![
        Message::user("question"),
        assistant,
        Message::tool("answered", "result"),
        Message::tool("retracted", "awaiting decision"),
        marker,
    ];

    strip_unpaired_tool_calls_from_view(&mut messages);

    let calls = messages[1].tool_calls.as_ref().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "answered");
    assert!(
        messages
            .iter()
            .all(|message| message.visibility == Visibility::All)
    );
}

#[test]
fn mark_produced_by_preserves_existing_metadata() {
    let mut msg = Message::assistant("hello").with_metadata(MessageMetadata {
        run_id: Some("existing-run".to_string()),
        step_index: Some(1),
    });

    msg.mark_produced_by("new-run", Some(2));

    assert_eq!(msg.produced_by_run_id(), Some("existing-run"));
    let metadata = msg.metadata.as_ref().unwrap();
    assert_eq!(metadata.step_index, Some(1));
}

#[test]
fn mark_produced_by_sets_missing_metadata() {
    let mut msg = Message::assistant("hello");

    msg.mark_produced_by("run-1", Some(0));

    assert_eq!(msg.produced_by_run_id(), Some("run-1"));
    let metadata = msg.metadata.as_ref().unwrap();
    assert_eq!(metadata.step_index, Some(0));
}

// ── Message text extraction tests ──

#[test]
fn test_message_text_multiblock() {
    let msg = Message::tool_with_content(
        "c1",
        vec![ContentBlock::text("first"), ContentBlock::text("second")],
    );
    assert_eq!(msg.text(), "firstsecond");
}

#[test]
fn test_message_text_empty_content() {
    let msg = Message {
        id: None,
        role: Role::User,
        content: vec![],
        tool_calls: None,
        tool_call_id: None,
        visibility: Visibility::All,
        metadata: None,
    };
    assert_eq!(msg.text(), "");
}

//! Integration tests verifying cross-module contract compatibility in awaken-contract.

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::event::{AgentEvent, WireEvent};
use awaken_contract::contract::inference::{
    ContextCompactionMode, ContextWindowPolicy, InferenceError, InferenceModelOverride,
    InferenceOverride, LLMResponse, ReasoningEffort, StopReason, StreamResult, TokenUsage,
};
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::message::{Message, MessageMetadata, Role, ToolCall, Visibility};
use awaken_contract::contract::storage::{
    MessageQuery, RunPage, RunQuery, RunRecord, StorageError,
};
use awaken_contract::contract::suspension::ToolCallOutcome;
use awaken_contract::contract::tool::{ToolDescriptor, ToolError, ToolResult};
use serde_json::json;

// ── Message <-> Event serialization roundtrips ─────────────────────

#[test]
fn message_embeds_in_run_finish_result_and_survives_roundtrip() {
    let msg = Message::assistant_with_tool_calls(
        "I'll search for that.",
        vec![ToolCall::new("c1", "search", json!({"query": "rust"}))],
    );
    let msg_json = serde_json::to_value(&msg).unwrap();

    let event = AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: Some(json!({"response": "done", "last_message": msg_json})),
        termination: TerminationReason::NaturalEnd,
    };

    let wire = serde_json::to_string(&event).unwrap();
    let parsed: AgentEvent = serde_json::from_str(&wire).unwrap();

    if let AgentEvent::RunFinish { result, .. } = parsed {
        let last_msg = result.as_ref().unwrap().get("last_message").unwrap();
        let restored: Message = serde_json::from_value(last_msg.clone()).unwrap();
        assert_eq!(restored.role, Role::Assistant);
        assert_eq!(restored.text(), "I'll search for that.");
        let calls = restored.tool_calls.unwrap();
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[0].arguments["query"], "rust");
    } else {
        panic!("expected RunFinish");
    }
}

#[test]
fn stream_event_wraps_tool_call_done_with_tool_result() {
    let result = ToolResult::success_with_message("calc", json!(42), "computed");
    let event = AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: result.clone(),
        outcome: ToolCallOutcome::Succeeded,
    };

    let se = WireEvent::new(5, "2026-01-01T00:00:00Z", event);
    let wire = serde_json::to_string(&se).unwrap();
    let parsed: WireEvent = serde_json::from_str(&wire).unwrap();

    assert_eq!(parsed.seq, 5);
    if let AgentEvent::ToolCallDone {
        result, outcome, ..
    } = &parsed.event
    {
        assert!(result.is_success());
        assert_eq!(result.data, json!(42));
        assert_eq!(result.message.as_deref(), Some("computed"));
        assert_eq!(*outcome, ToolCallOutcome::Succeeded);
    } else {
        panic!("expected ToolCallDone");
    }
}

#[test]
fn messages_snapshot_event_preserves_full_message_structure() {
    let messages = vec![
        Message::system("You are helpful"),
        Message::user("hello"),
        Message::assistant("hi"),
        Message::tool("c1", "result data"),
    ];

    let serialized: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect();

    let event = AgentEvent::MessagesSnapshot {
        messages: serialized,
    };

    let wire = serde_json::to_string(&event).unwrap();
    let parsed: AgentEvent = serde_json::from_str(&wire).unwrap();

    if let AgentEvent::MessagesSnapshot { messages: msgs } = parsed {
        assert_eq!(msgs.len(), 4);
        let restored: Message = serde_json::from_value(msgs[0].clone()).unwrap();
        assert_eq!(restored.role, Role::System);
        let restored: Message = serde_json::from_value(msgs[3].clone()).unwrap();
        assert_eq!(restored.role, Role::Tool);
        assert_eq!(restored.tool_call_id.as_deref(), Some("c1"));
    } else {
        panic!("expected MessagesSnapshot");
    }
}

// ── Tool result creation and serialization ─────────────────────────

#[test]
fn tool_result_variants_serialize_and_status_check() {
    let success = ToolResult::success("search", json!({"hits": 10}));
    assert!(success.is_success());
    assert!(!success.is_error());
    assert!(!success.is_pending());

    let error = ToolResult::error("search", "network failure");
    assert!(error.is_error());
    assert!(!error.is_success());
    assert_eq!(error.message.as_deref(), Some("network failure"));
    assert_eq!(error.data, serde_json::Value::Null);

    let suspended = ToolResult::suspended("approval", "waiting for user approval");
    assert!(suspended.is_pending());
    assert!(!suspended.is_success());
    assert!(!suspended.is_error());

    // All three roundtrip through JSON
    for result in [&success, &error, &suspended] {
        let json_val = result.to_json();
        let restored: ToolResult = serde_json::from_value(json_val).unwrap();
        assert_eq!(restored.tool_name, result.tool_name);
        assert_eq!(restored.status, result.status);
    }
}

#[test]
fn tool_result_error_with_code_has_structured_data() {
    let result = ToolResult::error_with_code("write", "PERMISSION_DENIED", "cannot write to /etc");
    assert!(result.is_error());
    assert_eq!(result.data["error"]["code"], "PERMISSION_DENIED");
    assert_eq!(result.data["error"]["message"], "cannot write to /etc");
    assert!(
        result
            .message
            .as_deref()
            .unwrap()
            .contains("PERMISSION_DENIED")
    );
}

#[test]
fn tool_result_in_event_roundtrip() {
    let result = ToolResult::error("fetch", "timeout");
    let event = AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result,
        outcome: ToolCallOutcome::Failed,
    };

    let wire = serde_json::to_string(&event).unwrap();
    let parsed: AgentEvent = serde_json::from_str(&wire).unwrap();

    if let AgentEvent::ToolCallDone {
        result, outcome, ..
    } = parsed
    {
        assert!(result.is_error());
        assert_eq!(outcome, ToolCallOutcome::Failed);
    } else {
        panic!("expected ToolCallDone");
    }
}

#[test]
fn tool_descriptor_serialization_roundtrip() {
    let desc = ToolDescriptor::new("search-1", "search", "Search the web").with_parameters(json!({
        "type": "object",
        "properties": {
            "query": {"type": "string"}
        },
        "required": ["query"]
    }));

    let wire = serde_json::to_string(&desc).unwrap();
    let parsed: ToolDescriptor = serde_json::from_str(&wire).unwrap();
    assert_eq!(parsed.name, "search");
    assert_eq!(parsed.parameters["properties"]["query"]["type"], "string");
}

// ── Storage contract validation patterns ───────────────────────────

#[test]
fn storage_error_variants_display_correctly() {
    let errors = vec![
        (
            StorageError::NotFound("thread-1".into()),
            "not found: thread-1",
        ),
        (
            StorageError::AlreadyExists("run-1".into()),
            "already exists: run-1",
        ),
        (
            StorageError::VersionConflict {
                expected: 3,
                actual: 5,
            },
            "version conflict: expected 3, actual 5",
        ),
        (StorageError::Io("disk full".into()), "io error: disk full"),
        (
            StorageError::Serialization("invalid utf8".into()),
            "serialization error: invalid utf8",
        ),
    ];

    for (err, expected) in errors {
        assert_eq!(err.to_string(), expected);
    }
}

#[test]
fn run_record_roundtrip_preserves_all_fields() {
    use awaken_contract::contract::lifecycle::RunStatus;

    let run = RunRecord {
        run_id: "r-1".into(),
        thread_id: "t-1".into(),
        agent_id: "agent-1".into(),
        parent_run_id: Some("r-parent".into()),
        status: RunStatus::Running,
        termination_code: Some("natural_end".into()),
        created_at: 1000,
        updated_at: 2000,
        steps: 5,
        input_tokens: 1000,
        output_tokens: 500,
        state: None,
    };

    let json = serde_json::to_string(&run).unwrap();
    let parsed: RunRecord = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.run_id, "r-1");
    assert_eq!(parsed.parent_run_id.as_deref(), Some("r-parent"));
    assert_eq!(parsed.steps, 5);
    assert_eq!(parsed.input_tokens, 1000);
    assert_eq!(parsed.output_tokens, 500);
    assert_eq!(parsed.termination_code.as_deref(), Some("natural_end"));
}

#[test]
fn run_page_with_multiple_records_roundtrips() {
    use awaken_contract::contract::lifecycle::RunStatus;

    let page = RunPage {
        items: vec![
            RunRecord {
                run_id: "r-1".into(),
                thread_id: "t-1".into(),
                agent_id: "a-1".into(),
                parent_run_id: None,
                status: RunStatus::Done,
                termination_code: None,
                created_at: 100,
                updated_at: 200,
                steps: 3,
                input_tokens: 500,
                output_tokens: 200,
                state: None,
            },
            RunRecord {
                run_id: "r-2".into(),
                thread_id: "t-1".into(),
                agent_id: "a-1".into(),
                parent_run_id: Some("r-1".into()),
                status: RunStatus::Running,
                termination_code: None,
                created_at: 300,
                updated_at: 400,
                steps: 1,
                input_tokens: 200,
                output_tokens: 100,
                state: None,
            },
        ],
        total: 5,
        has_more: true,
    };

    let json = serde_json::to_string(&page).unwrap();
    let parsed: RunPage = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.items.len(), 2);
    assert_eq!(parsed.total, 5);
    assert!(parsed.has_more);
}

#[test]
fn query_defaults_are_sensible() {
    let mq = MessageQuery::default();
    assert_eq!(mq.offset, 0);
    assert_eq!(mq.limit, 50);

    let rq = RunQuery::default();
    assert_eq!(rq.offset, 0);
    assert_eq!(rq.limit, 50);
    assert!(rq.thread_id.is_none());
    assert!(rq.status.is_none());
}

// ── Error type conversions between modules ───────────────────────���─

#[test]
fn tool_error_variants_display() {
    let errors: Vec<(ToolError, &str)> = vec![
        (
            ToolError::InvalidArguments("bad input".into()),
            "Invalid arguments: bad input",
        ),
        (
            ToolError::ExecutionFailed("crash".into()),
            "Execution failed: crash",
        ),
        (ToolError::Denied("blocked".into()), "Denied: blocked"),
        (
            ToolError::NotFound("missing tool".into()),
            "Not found: missing tool",
        ),
        (ToolError::Internal("oom".into()), "Internal error: oom"),
    ];
    for (err, expected) in errors {
        assert_eq!(err.to_string(), expected);
    }
}

#[test]
fn inference_error_embeds_in_llm_response_and_event() {
    let inf_err = InferenceError {
        error_type: "rate_limit".into(),
        message: "429 Too Many Requests".into(),
        error_class: Some("rate_limit".into()),
    };
    let response = LLMResponse::error(inf_err.clone());
    assert!(response.outcome.is_err());

    // The error can be serialized and embedded in an AgentEvent::Error
    let event = AgentEvent::Error {
        message: response.outcome.unwrap_err().message,
        code: Some("INFERENCE_ERROR".into()),
    };
    let wire = serde_json::to_string(&event).unwrap();
    let parsed: AgentEvent = serde_json::from_str(&wire).unwrap();
    if let AgentEvent::Error { message, code } = parsed {
        assert_eq!(message, "429 Too Many Requests");
        assert_eq!(code.as_deref(), Some("INFERENCE_ERROR"));
    } else {
        panic!("expected Error event");
    }
}

// ── Cross-module integration: inference + message + event ──────────

#[test]
fn stream_result_to_message_to_event_pipeline() {
    // Simulate: LLM produces StreamResult -> converted to Message -> emitted as event
    let stream_result = StreamResult {
        content: vec![ContentBlock::text("The answer is 42.")],
        tool_calls: vec![ToolCall::new("c1", "calculator", json!({"expr": "6*7"}))],
        usage: Some(TokenUsage {
            prompt_tokens: Some(100),
            completion_tokens: Some(20),
            total_tokens: Some(120),
            ..Default::default()
        }),
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    };

    assert!(stream_result.needs_tools());
    assert_eq!(stream_result.text(), "The answer is 42.");
    assert!(!stream_result.needs_truncation_recovery());

    // Convert to message
    let msg =
        Message::assistant_with_tool_calls(stream_result.text(), stream_result.tool_calls.clone())
            .with_metadata(MessageMetadata {
                run_id: Some("r1".into()),
                step_index: Some(0),
            });

    assert_eq!(msg.role, Role::Assistant);
    assert_eq!(msg.tool_calls.as_ref().unwrap().len(), 1);

    // Emit as inference complete event
    let event = AgentEvent::InferenceComplete {
        model: "gpt-4o".into(),
        usage: stream_result.usage.clone(),
        duration_ms: 500,
    };

    let wire = serde_json::to_string(&event).unwrap();
    let parsed: AgentEvent = serde_json::from_str(&wire).unwrap();
    if let AgentEvent::InferenceComplete { usage, .. } = parsed {
        let u = usage.unwrap();
        assert_eq!(u.prompt_tokens, Some(100));
        assert_eq!(u.total_tokens, Some(120));
    } else {
        panic!("expected InferenceComplete");
    }
}

// ── InferenceOverride cross-module compatibility ───────────────────

#[test]
fn inference_override_from_model_override_merges_with_params() {
    let model = InferenceModelOverride {
        model: "claude-opus".into(),
        fallback_models: vec!["claude-sonnet".into()],
    };

    let mut combined = InferenceOverride::from(model);
    assert!(!combined.is_empty());
    assert_eq!(combined.model.as_deref(), Some("claude-opus"));

    // Merge with parameter overrides
    combined.merge(InferenceOverride {
        temperature: Some(0.7),
        max_tokens: Some(4096),
        reasoning_effort: Some(ReasoningEffort::High),
        ..Default::default()
    });

    assert_eq!(combined.model.as_deref(), Some("claude-opus"));
    assert_eq!(combined.temperature, Some(0.7));
    assert_eq!(combined.max_tokens, Some(4096));
    assert_eq!(combined.reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(combined.fallback_models, Some(vec!["claude-sonnet".into()]));
}

#[test]
fn context_window_policy_serde_roundtrip_with_all_fields() {
    let policy = ContextWindowPolicy {
        max_context_tokens: 128_000,
        max_output_tokens: 8_192,
        min_recent_messages: 5,
        enable_prompt_cache: true,
        autocompact_threshold: Some(100_000),
        compaction_mode: ContextCompactionMode::CompactToSafeFrontier,
        compaction_raw_suffix_messages: 4,
    };

    let json = serde_json::to_value(&policy).unwrap();
    let restored: ContextWindowPolicy = serde_json::from_value(json).unwrap();

    assert_eq!(restored.max_context_tokens, 128_000);
    assert_eq!(
        restored.compaction_mode,
        ContextCompactionMode::CompactToSafeFrontier
    );
    assert_eq!(restored.compaction_raw_suffix_messages, 4);
}

// ── Message visibility integration with events ─────────────────────

#[test]
fn internal_messages_excluded_from_snapshot_by_visibility() {
    let messages = vec![
        Message::system("You are helpful"),
        Message::internal_system("hidden reminder"),
        Message::user("hello"),
    ];

    // Filter visible messages (simulating what a protocol layer would do)
    let visible: Vec<&Message> = messages
        .iter()
        .filter(|m| m.visibility == Visibility::All)
        .collect();

    assert_eq!(visible.len(), 2);
    assert_eq!(visible[0].role, Role::System);
    assert_eq!(visible[1].role, Role::User);

    // Internal message preserved when serialized
    let internal = &messages[1];
    let json = serde_json::to_string(internal).unwrap();
    assert!(json.contains("\"visibility\":\"internal\""));
    let restored: Message = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.visibility, Visibility::Internal);
}

// ── Multimodal content through the pipeline ────────────────────────

#[test]
fn multimodal_message_roundtrip_through_event() {
    let msg = Message::user_with_content(vec![
        ContentBlock::text("Look at this image:"),
        ContentBlock::image_url("https://example.com/cat.png"),
    ]);

    let msg_json = serde_json::to_value(&msg).unwrap();
    let event = AgentEvent::MessagesSnapshot {
        messages: vec![msg_json],
    };

    let wire = serde_json::to_string(&event).unwrap();
    let parsed: AgentEvent = serde_json::from_str(&wire).unwrap();

    if let AgentEvent::MessagesSnapshot { messages } = parsed {
        let restored: Message = serde_json::from_value(messages[0].clone()).unwrap();
        assert_eq!(restored.content.len(), 2);
        assert_eq!(restored.text(), "Look at this image:");
    } else {
        panic!("expected MessagesSnapshot");
    }
}

// ── Termination reason coverage ────────────────────────────────────

#[test]
fn all_termination_reasons_serialize_in_run_finish() {
    use awaken_contract::contract::lifecycle::StoppedReason;

    let reasons = vec![
        TerminationReason::NaturalEnd,
        TerminationReason::BehaviorRequested,
        TerminationReason::Stopped(StoppedReason::with_detail("max_turns", "hit 10 turns")),
        TerminationReason::Cancelled,
        TerminationReason::Blocked("permission denied".into()),
        TerminationReason::Suspended,
        TerminationReason::Error("fatal crash".into()),
    ];

    for reason in reasons {
        let event = AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: reason,
        };

        let wire = serde_json::to_string(&event).unwrap();
        let parsed: AgentEvent = serde_json::from_str(&wire).unwrap();
        assert!(matches!(parsed, AgentEvent::RunFinish { .. }));
    }
}

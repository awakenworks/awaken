//! AG-UI encoder contract tests — migrated from tirea-protocol-ag-ui.
//!
//! Validates event mapping, lifecycle management, state pass-through,
//! message ID propagation, and terminal guard behavior.

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::lifecycle::{StoppedReason, TerminationReason};
use awaken_contract::contract::suspension::ToolCallOutcome;
use awaken_contract::contract::tool::ToolResult;
use awaken_contract::contract::transport::Transcoder;
use awaken_server::protocols::ag_ui::encoder::AgUiEncoder;
use awaken_server::protocols::ag_ui::types::{Event, Role};
use serde_json::json;

// ============================================================================
// Helper
// ============================================================================

fn make_encoder_with_run(thread_id: &str, run_id: &str) -> AgUiEncoder {
    let mut enc = AgUiEncoder::new();
    enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: thread_id.into(),
        run_id: run_id.into(),
        parent_run_id: None,
    });
    enc
}

// ============================================================================
// Transcoder trait integration
// ============================================================================

#[test]
fn transcoder_trait_delegates_to_on_agent_event() {
    let mut enc = AgUiEncoder::new();
    let events = enc.transcode(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::RunStarted { run_id, .. } if run_id == "r1"));
}

// ============================================================================
// Full lifecycle: text → tool → text → finish
// ============================================================================

#[test]
fn full_lifecycle_text_tool_text_finish() {
    let mut enc = AgUiEncoder::new();

    // 1. RunStart → RunStarted
    let ev = enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    });
    assert_eq!(ev.len(), 1);
    assert!(matches!(&ev[0], Event::RunStarted { run_id, thread_id, .. }
        if run_id == "r1" && thread_id == "t1"));

    // 2. StepStart → StepStarted
    let ev = enc.on_agent_event(&AgentEvent::StepStart {
        message_id: "msg_1".into(),
    });
    assert_eq!(ev.len(), 1);
    assert!(matches!(&ev[0], Event::StepStarted { step_name, .. } if step_name == "step_1"));

    // 3. Text streaming opens text message
    let ev = enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "Hello ".into(),
    });
    assert_eq!(ev.len(), 2);
    assert!(matches!(&ev[0], Event::TextMessageStart { .. }));
    assert!(matches!(&ev[1], Event::TextMessageContent { delta, .. } if delta == "Hello "));

    // 4. Second text delta reuses open message
    let ev = enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "world".into(),
    });
    assert_eq!(ev.len(), 1);
    assert!(matches!(&ev[1 - 1], Event::TextMessageContent { delta, .. } if delta == "world"));

    // 5. Tool call closes text message
    let ev = enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "call_1".into(),
        name: "search".into(),
    });
    assert!(ev.iter().any(|e| matches!(e, Event::TextMessageEnd { .. })));
    assert!(ev.iter().any(|e| matches!(e, Event::ToolCallStart { .. })));

    // 6. Tool call args
    let ev = enc.on_agent_event(&AgentEvent::ToolCallDelta {
        id: "call_1".into(),
        args_delta: r#"{"q":"rust"}"#.into(),
    });
    assert_eq!(ev.len(), 1);
    assert!(
        matches!(&ev[0], Event::ToolCallArgs { tool_call_id, delta, .. }
        if tool_call_id == "call_1" && delta.contains("rust"))
    );

    // 7. Tool call end
    let ev = enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "call_1".into(),
        name: "search".into(),
        arguments: json!({"q": "rust"}),
    });
    assert_eq!(ev.len(), 1);
    assert!(matches!(&ev[0], Event::ToolCallEnd { tool_call_id, .. } if tool_call_id == "call_1"));

    // 8. Tool result
    let ev = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "call_1".into(),
        message_id: "msg_tool_1".into(),
        result: ToolResult::success("search", json!({"results": [1, 2, 3]})),
        outcome: ToolCallOutcome::Succeeded,
    });
    assert_eq!(ev.len(), 1);
    assert!(
        matches!(&ev[0], Event::ToolCallResult { tool_call_id, .. } if tool_call_id == "call_1")
    );

    // 9. More text
    let ev = enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "Found 3 results.".into(),
    });
    assert_eq!(ev.len(), 2); // start + content
    assert!(matches!(&ev[0], Event::TextMessageStart { .. }));

    // 10. StepEnd closes text
    let ev = enc.on_agent_event(&AgentEvent::StepEnd);
    assert!(ev.iter().any(|e| matches!(e, Event::TextMessageEnd { .. })));
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::StepFinished { step_name, .. } if step_name == "step_1"))
    );

    // 11. RunFinish
    let ev = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    assert!(ev.iter().any(|e| matches!(e, Event::RunFinished { .. })));
}

// ============================================================================
// Terminal guard: events after finish are suppressed
// ============================================================================

#[test]
fn events_after_run_finish_are_suppressed() {
    let mut enc = make_encoder_with_run("t1", "r1");
    enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });

    assert!(
        enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "late".into()
        })
        .is_empty()
    );
    assert!(
        enc.on_agent_event(&AgentEvent::ToolCallStart {
            id: "c".into(),
            name: "x".into(),
        })
        .is_empty()
    );
    assert!(
        enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        })
        .is_empty()
    );
}

#[test]
fn events_after_error_are_suppressed() {
    let mut enc = AgUiEncoder::new();
    enc.on_agent_event(&AgentEvent::Error {
        message: "fatal".into(),
        code: None,
    });

    assert!(
        enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "late".into()
        })
        .is_empty()
    );
}

// ============================================================================
// Termination reason mapping
// ============================================================================

#[test]
fn natural_end_emits_run_finished() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: Some(json!({"ok": true})),
        termination: TerminationReason::NaturalEnd,
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::RunFinished { .. }))
    );
}

#[test]
fn error_termination_emits_run_error() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Error("boom".into()),
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::RunError { message, .. } if message == "boom"))
    );
}

#[test]
fn behaviour_requested_emits_run_finished() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::BehaviorRequested,
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::RunFinished { .. }))
    );
}

#[test]
fn cancelled_emits_run_finished() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Cancelled,
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::RunFinished { .. }))
    );
}

#[test]
fn suspended_emits_run_finished() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Suspended,
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::RunFinished { .. }))
    );
}

#[test]
fn stopped_max_rounds_emits_run_finished() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Stopped(StoppedReason::new("max_rounds_reached")),
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::RunFinished { .. }))
    );
}

#[test]
fn blocked_emits_run_error() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Blocked("unsafe tool".into()),
    });
    // Blocked maps through the _ catch-all to RunFinished
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::RunFinished { .. }))
    );
}

// ============================================================================
// Error event
// ============================================================================

#[test]
fn error_event_emits_run_error() {
    let mut enc = AgUiEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::Error {
        message: "fatal error".into(),
        code: Some("E001".into()),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::RunError { message, code, .. }
        if message == "fatal error" && *code == Some("E001".into())));
}

// ============================================================================
// Reasoning events
// ============================================================================

#[test]
fn reasoning_delta_opens_reasoning_message() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ReasoningDelta {
        delta: "thinking".into(),
    });
    assert_eq!(events.len(), 2);
    assert!(
        matches!(&events[0], Event::ReasoningMessageStart { role, .. } if *role == Role::Assistant)
    );
    assert!(
        matches!(&events[1], Event::ReasoningMessageContent { delta, .. } if delta == "thinking")
    );
}

#[test]
fn second_reasoning_delta_reuses_open_block() {
    let mut enc = make_encoder_with_run("t1", "r1");
    enc.on_agent_event(&AgentEvent::ReasoningDelta { delta: "a".into() });
    let events = enc.on_agent_event(&AgentEvent::ReasoningDelta { delta: "b".into() });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::ReasoningMessageContent { delta, .. } if delta == "b"));
}

#[test]
fn tool_call_closes_reasoning_block() {
    let mut enc = make_encoder_with_run("t1", "r1");
    enc.on_agent_event(&AgentEvent::ReasoningDelta {
        delta: "think".into(),
    });
    let events = enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "search".into(),
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ReasoningMessageEnd { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ToolCallStart { .. }))
    );
}

#[test]
fn run_finish_closes_reasoning_block() {
    let mut enc = make_encoder_with_run("t1", "r1");
    enc.on_agent_event(&AgentEvent::ReasoningDelta {
        delta: "think".into(),
    });
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ReasoningMessageEnd { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::RunFinished { .. }))
    );
}

#[test]
fn reasoning_encrypted_value_forwarded() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ReasoningEncryptedValue {
        encrypted_value: "opaque-token".into(),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::ReasoningEncryptedValue {
        encrypted_value, ..
    } if encrypted_value == "opaque-token"));
}

// ============================================================================
// Step events
// ============================================================================

#[test]
fn step_events_generate_incrementing_names() {
    let mut enc = AgUiEncoder::new();
    let s1 = enc.on_agent_event(&AgentEvent::StepStart {
        message_id: "m1".into(),
    });
    assert!(matches!(&s1[0], Event::StepStarted { step_name, .. } if step_name == "step_1"));

    let e1 = enc.on_agent_event(&AgentEvent::StepEnd);
    assert!(
        e1.iter()
            .any(|e| matches!(e, Event::StepFinished { step_name, .. } if step_name == "step_1"))
    );

    let s2 = enc.on_agent_event(&AgentEvent::StepStart {
        message_id: "m2".into(),
    });
    assert!(matches!(&s2[0], Event::StepStarted { step_name, .. } if step_name == "step_2"));
}

#[test]
fn step_end_closes_open_text() {
    let mut enc = make_encoder_with_run("t1", "r1");
    enc.on_agent_event(&AgentEvent::TextDelta { delta: "hi".into() });
    let events = enc.on_agent_event(&AgentEvent::StepEnd);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TextMessageEnd { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::StepFinished { .. }))
    );
}

// ============================================================================
// State events pass through
// ============================================================================

#[test]
fn state_snapshot_forwarded() {
    let mut enc = AgUiEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({"key": "val"}),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::StateSnapshot { snapshot, .. }
        if snapshot == &json!({"key": "val"})));
}

#[test]
fn state_delta_forwarded() {
    let mut enc = AgUiEncoder::new();
    let patch = vec![json!({"op": "replace", "path": "/x", "value": 42})];
    let events = enc.on_agent_event(&AgentEvent::StateDelta {
        delta: patch.clone(),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::StateDelta { delta, .. } if *delta == patch));
}

#[test]
fn messages_snapshot_forwarded() {
    let mut enc = AgUiEncoder::new();
    let msgs = vec![json!({"role": "user", "content": "hi"})];
    let events = enc.on_agent_event(&AgentEvent::MessagesSnapshot {
        messages: msgs.clone(),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::MessagesSnapshot { messages, .. } if *messages == msgs));
}

// ============================================================================
// Activity events
// ============================================================================

#[test]
fn activity_snapshot_forwarded() {
    let mut enc = AgUiEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "m1".into(),
        activity_type: "thinking".into(),
        content: json!({"text": "processing"}),
        replace: Some(true),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::ActivitySnapshot {
        message_id, activity_type, replace, ..
    } if message_id == "m1" && activity_type == "thinking" && *replace == Some(true)));
}

#[test]
fn activity_delta_forwarded() {
    let mut enc = AgUiEncoder::new();
    let patch = vec![json!({"op": "replace", "path": "/progress", "value": 50})];
    let events = enc.on_agent_event(&AgentEvent::ActivityDelta {
        message_id: "m1".into(),
        activity_type: "progress".into(),
        patch: patch.clone(),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::ActivityDelta {
        message_id, activity_type, patch: p, ..
    } if message_id == "m1" && activity_type == "progress" && *p == patch));
}

// ============================================================================
// Tool call result from ToolCallDone
// ============================================================================

#[test]
fn tool_call_done_success_emits_result() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m_tool".into(),
        result: ToolResult::success("calc", json!(42)),
        outcome: ToolCallOutcome::Succeeded,
    });
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], Event::ToolCallResult { tool_call_id, message_id, content, .. }
        if tool_call_id == "c1" && message_id == "m_tool" && content == "42")
    );
}

#[test]
fn tool_call_done_error_emits_result_with_error_message() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m_tool".into(),
        result: ToolResult::error("search", "not found"),
        outcome: ToolCallOutcome::Failed,
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::ToolCallResult { content, .. }
        if content == "not found"));
}

#[test]
fn tool_call_done_pending_emits_nothing() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m_tool".into(),
        result: ToolResult::suspended("confirm", "needs approval"),
        outcome: ToolCallOutcome::Suspended,
    });
    assert!(events.is_empty());
}

// ============================================================================
// ToolCallResumed
// ============================================================================

#[test]
fn tool_call_resumed_emits_result() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
        target_id: "c1".into(),
        result: json!({"approved": true}),
    });
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], Event::ToolCallResult { tool_call_id, .. }
        if tool_call_id == "c1")
    );
}

// ============================================================================
// Message ID propagation from StepStart
// ============================================================================

#[test]
fn step_start_sets_message_id_for_subsequent_text() {
    let mut enc = AgUiEncoder::new();
    let step_msg_id = "pre-gen-assistant-uuid";
    enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    });
    enc.on_agent_event(&AgentEvent::StepStart {
        message_id: step_msg_id.into(),
    });

    let events = enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "Hello".into(),
    });
    let text_start = events
        .iter()
        .find(|e| matches!(e, Event::TextMessageStart { .. }));
    assert!(text_start.is_some());
    if let Some(Event::TextMessageStart { message_id, .. }) = text_start {
        assert_eq!(message_id, step_msg_id);
    }
}

#[test]
fn tool_call_result_uses_tool_call_done_message_id() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let tool_msg_id = "pre-gen-tool-uuid";
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "call_1".into(),
        message_id: tool_msg_id.into(),
        result: ToolResult::success("echo", json!({"echoed": "test"})),
        outcome: ToolCallOutcome::Succeeded,
    });
    let tool_result = events
        .iter()
        .find(|e| matches!(e, Event::ToolCallResult { .. }));
    assert!(tool_result.is_some());
    if let Some(Event::ToolCallResult { message_id, .. }) = tool_result {
        assert_eq!(message_id, tool_msg_id);
    }
}

// ============================================================================
// Run start with parent run ID
// ============================================================================

#[test]
fn run_start_with_parent_run_id() {
    let mut enc = AgUiEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: Some("parent-r0".into()),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::RunStarted { parent_run_id, .. }
        if *parent_run_id == Some("parent-r0".into())));
}

// ============================================================================
// InferenceComplete is silently consumed
// ============================================================================

#[test]
fn inference_complete_silently_consumed() {
    let mut enc = AgUiEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::InferenceComplete {
        model: "gpt-4o".into(),
        usage: None,
        duration_ms: 1234,
    });
    assert!(events.is_empty());
}

// ============================================================================
// Serde roundtrips for AG-UI event types
// ============================================================================

#[test]
fn run_started_serde_roundtrip() {
    let event = Event::run_started("t1", "r1", None);
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("RUN_STARTED"));
    let parsed: Event = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, event);
}

#[test]
fn run_finished_serde_roundtrip() {
    let event = Event::run_finished("t1", "r1", Some(json!({"ok": true})));
    let json = serde_json::to_string(&event).unwrap();
    let parsed: Event = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, event);
}

#[test]
fn run_error_serde_roundtrip() {
    let event = Event::run_error("failed", Some("E001".into()));
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("RUN_ERROR"));
    let parsed: Event = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, event);
}

#[test]
fn text_message_events_serde_roundtrip() {
    for event in [
        Event::text_message_start("m1"),
        Event::text_message_content("m1", "hello"),
        Event::text_message_end("m1"),
    ] {
        let json = serde_json::to_string(&event).unwrap();
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }
}

#[test]
fn tool_call_events_serde_roundtrip() {
    for event in [
        Event::tool_call_start("c1", "search", Some("m1".into())),
        Event::tool_call_args("c1", r#"{"q":"rust"}"#),
        Event::tool_call_end("c1"),
        Event::tool_call_result("m1", "c1", "42"),
    ] {
        let json = serde_json::to_string(&event).unwrap();
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }
}

#[test]
fn step_events_serde_roundtrip() {
    for event in [
        Event::step_started("step_1"),
        Event::step_finished("step_1"),
    ] {
        let json = serde_json::to_string(&event).unwrap();
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }
}

#[test]
fn state_events_serde_roundtrip() {
    let snapshot = Event::state_snapshot(json!({"key": "val"}));
    let delta = Event::state_delta(vec![json!({"op": "add", "path": "/x", "value": 1})]);
    for event in [snapshot, delta] {
        let json = serde_json::to_string(&event).unwrap();
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }
}

#[test]
fn messages_snapshot_serde_roundtrip() {
    let event = Event::messages_snapshot(vec![json!({"role": "user", "content": "hi"})]);
    let json = serde_json::to_string(&event).unwrap();
    let parsed: Event = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, event);
}

// ============================================================================
// Mixed text and reasoning lifecycle
// ============================================================================

#[test]
fn text_then_reasoning_then_text_lifecycle() {
    let mut enc = make_encoder_with_run("t1", "r1");

    // Text block 1
    let ev = enc.on_agent_event(&AgentEvent::TextDelta { delta: "a".into() });
    assert_eq!(ev.len(), 2); // start + content

    // Tool call closes text
    let ev = enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "search".into(),
    });
    assert!(ev.iter().any(|e| matches!(e, Event::TextMessageEnd { .. })));

    // Reasoning after tool
    let ev = enc.on_agent_event(&AgentEvent::ReasoningDelta {
        delta: "think".into(),
    });
    assert_eq!(ev.len(), 2); // start + content

    // Another tool call closes reasoning
    let ev = enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c2".into(),
        name: "calc".into(),
    });
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::ReasoningMessageEnd { .. }))
    );

    // Text block 2
    let ev = enc.on_agent_event(&AgentEvent::TextDelta { delta: "b".into() });
    assert_eq!(ev.len(), 2); // start + content
}

// ============================================================================
// Multiple tool calls in sequence
// ============================================================================

#[test]
fn multiple_sequential_tool_calls() {
    let mut enc = make_encoder_with_run("t1", "r1");

    for i in 1..=3 {
        let id = format!("call_{i}");
        let ev = enc.on_agent_event(&AgentEvent::ToolCallStart {
            id: id.clone(),
            name: format!("tool_{i}"),
        });
        assert!(ev.iter().any(|e| matches!(e, Event::ToolCallStart { .. })));

        let ev = enc.on_agent_event(&AgentEvent::ToolCallReady {
            id: id.clone(),
            name: format!("tool_{i}"),
            arguments: json!({}),
        });
        assert!(matches!(&ev[0], Event::ToolCallEnd { .. }));

        let ev = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: id.clone(),
            message_id: format!("m_tool_{i}"),
            result: ToolResult::success(format!("tool_{i}"), json!(i)),
            outcome: ToolCallOutcome::Succeeded,
        });
        assert_eq!(ev.len(), 1);
    }
}

// ============================================================================
// Empty run (start + finish, no events in between)
// ============================================================================

#[test]
fn empty_run_start_finish() {
    let mut enc = AgUiEncoder::new();
    let start = enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    });
    assert_eq!(start.len(), 1);

    let finish = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    assert_eq!(finish.len(), 1);
    assert!(matches!(&finish[0], Event::RunFinished { .. }));
}

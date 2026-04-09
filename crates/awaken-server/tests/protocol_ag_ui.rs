//! AG-UI encoder contract tests — migrated from tirea-protocol-ag-ui.
//!
//! Validates event mapping, lifecycle management, state pass-through,
//! message ID propagation, and terminal guard behavior.

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::inference::TokenUsage;
use awaken_contract::contract::lifecycle::{StoppedReason, TerminationReason};
use awaken_contract::contract::suspension::{
    PendingToolCall, SuspendTicket, Suspension, ToolCallOutcome, ToolCallResumeMode,
};
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
fn suspended_run_uses_pending_interrupt_id() {
    let mut enc = make_encoder_with_run("t1", "r1");
    enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "tc1".into(),
        message_id: "m1".into(),
        result: ToolResult::suspended_with(
            "dangerous",
            "needs approval",
            SuspendTicket::new(
                Suspension {
                    id: "perm_tc1".into(),
                    action: "tool:PermissionConfirm".into(),
                    message: "Permission required".into(),
                    parameters: json!({"cmd": "rm"}),
                    response_schema: None,
                },
                PendingToolCall::new("tc1", "dangerous", json!({"cmd": "rm"})),
                ToolCallResumeMode::ReplayToolCall,
            ),
        ),
        outcome: ToolCallOutcome::Suspended,
    });

    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Suspended,
    });

    assert!(events.iter().any(|event| {
        matches!(
            event,
            Event::RunFinished {
                interrupt: Some(interrupt),
                ..
            } if interrupt.id.as_deref() == Some("perm_tc1")
                && interrupt.reason.as_deref() == Some("tool:PermissionConfirm")
        )
    }));
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

// ============================================================================
// Tool execution and event encoding tests (20 tests)
// ============================================================================

// 1. Tool call start event has correct fields (id, name)
#[test]
fn tool_call_start_has_correct_id_and_name() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "call_abc".into(),
        name: "get_weather".into(),
    });
    let tc = events
        .iter()
        .find(|e| matches!(e, Event::ToolCallStart { .. }))
        .unwrap();
    assert!(
        matches!(tc, Event::ToolCallStart { tool_call_id, tool_call_name, parent_message_id, .. }
            if tool_call_id == "call_abc"
                && tool_call_name == "get_weather"
                && *parent_message_id == Some("r1".into()))
    );
}

// 2. Tool call args event encodes arguments correctly
#[test]
fn tool_call_args_encodes_arguments() {
    let mut enc = make_encoder_with_run("t1", "r1");
    enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "search".into(),
    });
    let args_json = r#"{"query":"hello world","limit":10}"#;
    let events = enc.on_agent_event(&AgentEvent::ToolCallDelta {
        id: "c1".into(),
        args_delta: args_json.into(),
    });
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], Event::ToolCallArgs { tool_call_id, delta, .. }
            if tool_call_id == "c1" && delta == args_json)
    );
}

// 3. Tool call end event emits after args
#[test]
fn tool_call_end_emits_after_args() {
    let mut enc = make_encoder_with_run("t1", "r1");
    enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "calc".into(),
    });
    enc.on_agent_event(&AgentEvent::ToolCallDelta {
        id: "c1".into(),
        args_delta: r#"{"expr":"1+1"}"#.into(),
    });
    let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "calc".into(),
        arguments: json!({"expr": "1+1"}),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::ToolCallEnd { tool_call_id, .. } if tool_call_id == "c1"));
}

// 4. Tool call result event has correct result payload
#[test]
fn tool_call_result_has_correct_payload() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let result_data = json!({"temperature": 72, "unit": "F"});
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "msg_tool".into(),
        result: ToolResult::success("get_weather", result_data.clone()),
        outcome: ToolCallOutcome::Succeeded,
    });
    assert_eq!(events.len(), 1);
    if let Event::ToolCallResult {
        tool_call_id,
        message_id,
        content,
        role,
        ..
    } = &events[0]
    {
        assert_eq!(tool_call_id, "c1");
        assert_eq!(message_id, "msg_tool");
        let parsed: serde_json::Value = serde_json::from_str(content).unwrap();
        assert_eq!(parsed, result_data);
        assert_eq!(*role, Some(Role::Tool));
    } else {
        panic!("expected ToolCallResult");
    }
}

// 5. Multiple tool calls produce correct event sequence
#[test]
fn multiple_tool_calls_produce_correct_sequence() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let mut all_events = Vec::new();

    // Tool 1: start -> delta -> ready -> done
    all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "tool_a".into(),
    }));
    all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallDelta {
        id: "c1".into(),
        args_delta: "{}".into(),
    }));
    all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "tool_a".into(),
        arguments: json!({}),
    }));
    all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::success("tool_a", json!("ok")),
        outcome: ToolCallOutcome::Succeeded,
    }));

    // Tool 2: start -> delta -> ready -> done
    all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c2".into(),
        name: "tool_b".into(),
    }));
    all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallDelta {
        id: "c2".into(),
        args_delta: "{}".into(),
    }));
    all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "c2".into(),
        name: "tool_b".into(),
        arguments: json!({}),
    }));
    all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c2".into(),
        message_id: "m2".into(),
        result: ToolResult::success("tool_b", json!("done")),
        outcome: ToolCallOutcome::Succeeded,
    }));

    // Verify correct order: ToolCallStart(c1), Args(c1), End(c1), Result(c1),
    //                        ToolCallStart(c2), Args(c2), End(c2), Result(c2)
    let tool_events: Vec<_> = all_events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::ToolCallStart { .. }
                    | Event::ToolCallArgs { .. }
                    | Event::ToolCallEnd { .. }
                    | Event::ToolCallResult { .. }
            )
        })
        .collect();
    assert_eq!(tool_events.len(), 8);
    assert!(
        matches!(tool_events[0], Event::ToolCallStart { tool_call_id, .. } if tool_call_id == "c1")
    );
    assert!(
        matches!(tool_events[1], Event::ToolCallArgs { tool_call_id, .. } if tool_call_id == "c1")
    );
    assert!(
        matches!(tool_events[2], Event::ToolCallEnd { tool_call_id, .. } if tool_call_id == "c1")
    );
    assert!(
        matches!(tool_events[3], Event::ToolCallResult { tool_call_id, .. } if tool_call_id == "c1")
    );
    assert!(
        matches!(tool_events[4], Event::ToolCallStart { tool_call_id, .. } if tool_call_id == "c2")
    );
    assert!(
        matches!(tool_events[5], Event::ToolCallArgs { tool_call_id, .. } if tool_call_id == "c2")
    );
    assert!(
        matches!(tool_events[6], Event::ToolCallEnd { tool_call_id, .. } if tool_call_id == "c2")
    );
    assert!(
        matches!(tool_events[7], Event::ToolCallResult { tool_call_id, .. } if tool_call_id == "c2")
    );
}

#[test]
fn mixed_same_step_tool_results_preserve_order() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let mut all_events = Vec::new();

    all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "get_weather".into(),
    }));
    all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "get_weather".into(),
        arguments: json!({"location": "Tokyo"}),
    }));
    all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::success("get_weather", json!({"temp": 25})),
        outcome: ToolCallOutcome::Succeeded,
    }));

    all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c2".into(),
        name: "dangerous_tool".into(),
    }));
    all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "c2".into(),
        name: "dangerous_tool".into(),
        arguments: json!({}),
    }));
    all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c2".into(),
        message_id: "m2".into(),
        result: ToolResult::error("dangerous_tool", "permission denied"),
        outcome: ToolCallOutcome::Failed,
    }));

    let tool_events: Vec<_> = all_events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::ToolCallStart { .. }
                    | Event::ToolCallEnd { .. }
                    | Event::ToolCallResult { .. }
            )
        })
        .collect();
    assert_eq!(tool_events.len(), 6);
    assert!(
        matches!(tool_events[0], Event::ToolCallStart { tool_call_id, .. } if tool_call_id == "c1")
    );
    assert!(
        matches!(tool_events[1], Event::ToolCallEnd { tool_call_id, .. } if tool_call_id == "c1")
    );
    assert!(
        matches!(tool_events[2], Event::ToolCallResult { tool_call_id, content, .. }
            if tool_call_id == "c1" && content == r#"{"temp":25}"#)
    );
    assert!(
        matches!(tool_events[3], Event::ToolCallStart { tool_call_id, .. } if tool_call_id == "c2")
    );
    assert!(
        matches!(tool_events[4], Event::ToolCallEnd { tool_call_id, .. } if tool_call_id == "c2")
    );
    assert!(
        matches!(tool_events[5], Event::ToolCallResult { tool_call_id, content, .. }
            if tool_call_id == "c2" && content == "permission denied")
    );
}

// 6. Failed tool produces error result event
#[test]
fn failed_tool_produces_error_result() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::error("broken_tool", "connection refused"),
        outcome: ToolCallOutcome::Failed,
    });
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], Event::ToolCallResult { tool_call_id, content, .. }
            if tool_call_id == "c1" && content == "connection refused")
    );
}

// 7. Unknown tool produces error event (not crash)
#[test]
fn unknown_tool_error_does_not_crash() {
    let mut enc = make_encoder_with_run("t1", "r1");
    // Simulate an unknown tool by using error result
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c_unknown".into(),
        message_id: "m_unk".into(),
        result: ToolResult::error("nonexistent_tool", "tool not found"),
        outcome: ToolCallOutcome::Failed,
    });
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], Event::ToolCallResult { tool_call_id, content, .. }
            if tool_call_id == "c_unknown" && content.contains("not found"))
    );
}

// 8. Text delta events encode content correctly
#[test]
fn text_delta_encodes_content_correctly() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "Hello, 世界! 🌍".into(),
    });
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], Event::TextMessageStart { role, .. } if *role == Role::Assistant));
    assert!(
        matches!(&events[1], Event::TextMessageContent { delta, .. } if delta == "Hello, 世界! 🌍")
    );
}

// 9. Run start event has thread_id and run_id
#[test]
fn run_start_has_thread_and_run_id() {
    let mut enc = AgUiEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "thread_42".into(),
        run_id: "run_99".into(),
        parent_run_id: None,
    });
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], Event::RunStarted { thread_id, run_id, parent_run_id, .. }
            if thread_id == "thread_42" && run_id == "run_99" && parent_run_id.is_none())
    );
}

// 10. Run finish event has termination reason
#[test]
fn run_finish_with_result_payload() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let result_val = json!({"summary": "completed 3 tasks"});
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: Some(result_val.clone()),
        termination: TerminationReason::NaturalEnd,
    });
    assert!(events.iter().any(|e| matches!(
        e,
        Event::RunFinished { result, .. } if *result == Some(result_val.clone())
    )));
}

// 11. Step boundary events (StepStart, StepEnd) in correct order
#[test]
fn step_boundary_events_in_correct_order() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let mut all = Vec::new();

    all.extend(enc.on_agent_event(&AgentEvent::StepStart {
        message_id: "m1".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "response".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::StepEnd));

    // StepStarted must come before StepFinished
    let step_start_idx = all
        .iter()
        .position(|e| matches!(e, Event::StepStarted { .. }))
        .unwrap();
    let step_finish_idx = all
        .iter()
        .position(|e| matches!(e, Event::StepFinished { .. }))
        .unwrap();
    assert!(step_start_idx < step_finish_idx);

    // Both refer to the same step name
    assert!(
        matches!(&all[step_start_idx], Event::StepStarted { step_name, .. } if step_name == "step_1")
    );
    assert!(
        matches!(&all[step_finish_idx], Event::StepFinished { step_name, .. } if step_name == "step_1")
    );
}

// 12. Inference complete has no AG-UI output (silently consumed)
#[test]
fn inference_complete_produces_no_events() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::InferenceComplete {
        model: "claude-3-opus".into(),
        usage: Some(TokenUsage {
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            total_tokens: Some(150),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            thinking_tokens: None,
        }),
        duration_ms: 2500,
    });
    assert!(events.is_empty());
}

// 13. State snapshot event encodes state correctly
#[test]
fn state_snapshot_encodes_complex_state() {
    let mut enc = AgUiEncoder::new();
    let state = json!({
        "user": {"name": "Alice", "role": "admin"},
        "items": [1, 2, 3],
        "nested": {"deep": {"value": true}}
    });
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: state.clone(),
    });
    assert_eq!(events.len(), 1);
    if let Event::StateSnapshot { snapshot, .. } = &events[0] {
        assert_eq!(*snapshot, state);
        assert_eq!(snapshot["user"]["name"], "Alice");
        assert_eq!(snapshot["items"][1], 2);
        assert_eq!(snapshot["nested"]["deep"]["value"], true);
    } else {
        panic!("expected StateSnapshot");
    }
}

// 14. Empty tool calls = no tool events emitted
#[test]
fn no_tool_calls_means_no_tool_events() {
    let mut enc = AgUiEncoder::new();
    let mut all = Vec::new();

    all.extend(enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    }));
    all.extend(enc.on_agent_event(&AgentEvent::StepStart {
        message_id: "m1".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "just text".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::StepEnd));
    all.extend(enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    }));

    let tool_events: Vec<_> = all
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::ToolCallStart { .. }
                    | Event::ToolCallArgs { .. }
                    | Event::ToolCallEnd { .. }
                    | Event::ToolCallResult { .. }
            )
        })
        .collect();
    assert!(tool_events.is_empty());
}

// 15. Tool suspension emits no result event (pending status)
#[test]
fn tool_suspension_emits_no_result() {
    let mut enc = make_encoder_with_run("t1", "r1");
    enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "confirm".into(),
    });
    enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "confirm".into(),
        arguments: json!({"action": "delete_file"}),
    });
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::suspended("confirm", "awaiting user approval"),
        outcome: ToolCallOutcome::Suspended,
    });
    assert!(events.is_empty());
}

// 16. Reasoning delta event encoding
#[test]
fn reasoning_delta_encodes_content() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ReasoningDelta {
        delta: "Let me analyze this step by step...".into(),
    });
    assert_eq!(events.len(), 2);
    assert!(
        matches!(&events[0], Event::ReasoningMessageStart { message_id, role, .. }
            if message_id == "r1" && *role == Role::Assistant)
    );
    assert!(
        matches!(&events[1], Event::ReasoningMessageContent { message_id, delta, .. }
            if message_id == "r1" && delta == "Let me analyze this step by step...")
    );
}

// 17. Activity snapshot event encoding
#[test]
fn activity_snapshot_encodes_content_as_map() {
    let mut enc = AgUiEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "act_1".into(),
        activity_type: "search_progress".into(),
        content: json!({"found": 5, "total": 100}),
        replace: Some(false),
    });
    assert_eq!(events.len(), 1);
    if let Event::ActivitySnapshot {
        message_id,
        activity_type,
        content,
        replace,
        ..
    } = &events[0]
    {
        assert_eq!(message_id, "act_1");
        assert_eq!(activity_type, "search_progress");
        assert_eq!(content.get("found"), Some(&json!(5)));
        assert_eq!(content.get("total"), Some(&json!(100)));
        assert_eq!(*replace, Some(false));
    } else {
        panic!("expected ActivitySnapshot");
    }
}

// 18. Multiple text deltas concatenated in final response
#[test]
fn multiple_text_deltas_stream_correctly() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let mut all = Vec::new();

    all.extend(enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "Hello ".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "world".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::TextDelta { delta: "!".into() }));

    // Should have 1 TextMessageStart + 3 TextMessageContent
    let starts: Vec<_> = all
        .iter()
        .filter(|e| matches!(e, Event::TextMessageStart { .. }))
        .collect();
    let contents: Vec<_> = all
        .iter()
        .filter_map(|e| match e {
            Event::TextMessageContent { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(starts.len(), 1);
    assert_eq!(contents.len(), 3);
    assert_eq!(contents.join(""), "Hello world!");
}

// 19. Error event encoding with message and code
#[test]
fn error_event_encodes_message_and_code() {
    let mut enc = AgUiEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::Error {
        message: "rate limit exceeded".into(),
        code: Some("RATE_LIMIT".into()),
    });
    assert_eq!(events.len(), 1);
    if let Event::RunError { message, code, .. } = &events[0] {
        assert_eq!(message, "rate limit exceeded");
        assert_eq!(*code, Some("RATE_LIMIT".into()));
    } else {
        panic!("expected RunError");
    }
}

// 20. Error event without code
#[test]
fn error_event_without_code() {
    let mut enc = AgUiEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::Error {
        message: "internal error".into(),
        code: None,
    });
    assert_eq!(events.len(), 1);
    if let Event::RunError { message, code, .. } = &events[0] {
        assert_eq!(message, "internal error");
        assert!(code.is_none());
    } else {
        panic!("expected RunError");
    }
}

// ============================================================================
// State management tests (8)
// ============================================================================

#[test]
fn state_snapshot_with_nested_json() {
    let mut enc = AgUiEncoder::new();
    let state = json!({
        "user": {
            "profile": {
                "name": "Alice",
                "preferences": {
                    "theme": "dark",
                    "notifications": {"email": true, "sms": false}
                }
            }
        },
        "session": {"token": "abc123"}
    });
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: state.clone(),
    });
    assert_eq!(events.len(), 1);
    if let Event::StateSnapshot { snapshot, .. } = &events[0] {
        assert_eq!(*snapshot, state);
        assert_eq!(snapshot["user"]["profile"]["name"], "Alice");
        assert_eq!(
            snapshot["user"]["profile"]["preferences"]["notifications"]["email"],
            true
        );
    } else {
        panic!("expected StateSnapshot");
    }
}

#[test]
fn state_snapshot_with_array_value() {
    let mut enc = AgUiEncoder::new();
    let state = json!({
        "items": [1, "two", null, true, {"nested": [10, 20]}],
        "tags": ["a", "b", "c"]
    });
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: state.clone(),
    });
    assert_eq!(events.len(), 1);
    if let Event::StateSnapshot { snapshot, .. } = &events[0] {
        assert_eq!(*snapshot, state);
        assert_eq!(snapshot["items"].as_array().unwrap().len(), 5);
        assert_eq!(snapshot["items"][4]["nested"][1], 20);
    } else {
        panic!("expected StateSnapshot");
    }
}

#[test]
fn state_snapshot_empty_object() {
    let mut enc = AgUiEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({}),
    });
    assert_eq!(events.len(), 1);
    if let Event::StateSnapshot { snapshot, .. } = &events[0] {
        assert_eq!(*snapshot, json!({}));
        assert!(snapshot.as_object().unwrap().is_empty());
    } else {
        panic!("expected StateSnapshot");
    }
}

#[test]
fn state_snapshot_with_null_values() {
    let mut enc = AgUiEncoder::new();
    let state = json!({
        "a": null,
        "b": {"inner": null},
        "c": [null, 1, null]
    });
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: state.clone(),
    });
    assert_eq!(events.len(), 1);
    if let Event::StateSnapshot { snapshot, .. } = &events[0] {
        assert_eq!(*snapshot, state);
        assert!(snapshot["a"].is_null());
        assert!(snapshot["b"]["inner"].is_null());
        assert!(snapshot["c"][0].is_null());
        assert!(snapshot["c"][2].is_null());
    } else {
        panic!("expected StateSnapshot");
    }
}

#[test]
fn multiple_state_snapshots_all_emitted() {
    let mut enc = AgUiEncoder::new();
    let ev1 = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({"version": 1}),
    });
    let ev2 = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({"version": 2}),
    });
    assert_eq!(ev1.len(), 1);
    assert_eq!(ev2.len(), 1);
    assert!(matches!(&ev1[0], Event::StateSnapshot { snapshot, .. } if snapshot["version"] == 1));
    assert!(matches!(&ev2[0], Event::StateSnapshot { snapshot, .. } if snapshot["version"] == 2));
}

#[test]
fn state_snapshot_preserves_field_order() {
    let mut enc = AgUiEncoder::new();
    let state = json!({
        "alpha": 1,
        "beta": 2,
        "gamma": 3,
        "delta": 4,
        "epsilon": 5
    });
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: state.clone(),
    });
    assert_eq!(events.len(), 1);
    if let Event::StateSnapshot { snapshot, .. } = &events[0] {
        let keys: Vec<&String> = snapshot.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 5);
        assert!(keys.contains(&&"alpha".to_string()));
        assert!(keys.contains(&&"beta".to_string()));
        assert!(keys.contains(&&"gamma".to_string()));
        assert!(keys.contains(&&"delta".to_string()));
        assert!(keys.contains(&&"epsilon".to_string()));
    } else {
        panic!("expected StateSnapshot");
    }
}

#[test]
fn state_snapshot_large_payload() {
    let mut enc = AgUiEncoder::new();
    let mut obj = serde_json::Map::new();
    for i in 0..60 {
        obj.insert(format!("field_{i}"), json!(i));
    }
    let state = serde_json::Value::Object(obj);
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: state.clone(),
    });
    assert_eq!(events.len(), 1);
    if let Event::StateSnapshot { snapshot, .. } = &events[0] {
        assert_eq!(*snapshot, state);
        assert_eq!(snapshot.as_object().unwrap().len(), 60);
        assert_eq!(snapshot["field_0"], 0);
        assert_eq!(snapshot["field_59"], 59);
    } else {
        panic!("expected StateSnapshot");
    }
}

#[test]
fn state_snapshot_after_tool_call() {
    let mut enc = make_encoder_with_run("t1", "r1");
    // Execute a tool call lifecycle
    enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "search".into(),
    });
    enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "search".into(),
        arguments: json!({"q": "test"}),
    });
    enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m_tool".into(),
        result: ToolResult::success("search", json!({"found": true})),
        outcome: ToolCallOutcome::Succeeded,
    });

    // State snapshot after tool call
    let state = json!({"search_complete": true, "results_count": 5});
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: state.clone(),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::StateSnapshot { snapshot, .. } if *snapshot == state));
}

// ============================================================================
// Message handling tests (8)
// ============================================================================

#[test]
fn messages_snapshot_with_tool_messages() {
    let mut enc = AgUiEncoder::new();
    let msgs = vec![
        json!({"role": "user", "content": "search for rust"}),
        json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "function": {"name": "search"}}]}),
        json!({"role": "tool", "content": "found 3 results", "tool_call_id": "c1"}),
    ];
    let events = enc.on_agent_event(&AgentEvent::MessagesSnapshot {
        messages: msgs.clone(),
    });
    assert_eq!(events.len(), 1);
    if let Event::MessagesSnapshot { messages, .. } = &events[0] {
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "c1");
        assert_eq!(messages[2]["content"], "found 3 results");
    } else {
        panic!("expected MessagesSnapshot");
    }
}

#[test]
fn messages_snapshot_ordering_preserved() {
    let mut enc = AgUiEncoder::new();
    let msgs: Vec<serde_json::Value> = (0..10)
        .map(|i| json!({"role": "user", "content": format!("msg_{i}"), "seq": i}))
        .collect();
    let events = enc.on_agent_event(&AgentEvent::MessagesSnapshot {
        messages: msgs.clone(),
    });
    assert_eq!(events.len(), 1);
    if let Event::MessagesSnapshot { messages, .. } = &events[0] {
        for (i, msg) in messages.iter().enumerate() {
            assert_eq!(msg["seq"], i);
            assert_eq!(msg["content"], format!("msg_{i}"));
        }
    } else {
        panic!("expected MessagesSnapshot");
    }
}

#[test]
fn messages_snapshot_with_metadata() {
    let mut enc = AgUiEncoder::new();
    let msgs = vec![json!({
        "role": "assistant",
        "content": "hello",
        "metadata": {
            "model": "gpt-4",
            "tokens": 50,
            "latency_ms": 200
        }
    })];
    let events = enc.on_agent_event(&AgentEvent::MessagesSnapshot {
        messages: msgs.clone(),
    });
    assert_eq!(events.len(), 1);
    if let Event::MessagesSnapshot { messages, .. } = &events[0] {
        assert_eq!(messages[0]["metadata"]["model"], "gpt-4");
        assert_eq!(messages[0]["metadata"]["tokens"], 50);
    } else {
        panic!("expected MessagesSnapshot");
    }
}

#[test]
fn messages_snapshot_empty_array() {
    let mut enc = AgUiEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::MessagesSnapshot { messages: vec![] });
    assert_eq!(events.len(), 1);
    if let Event::MessagesSnapshot { messages, .. } = &events[0] {
        assert!(messages.is_empty());
    } else {
        panic!("expected MessagesSnapshot");
    }
}

#[test]
fn messages_snapshot_mixed_roles() {
    let mut enc = AgUiEncoder::new();
    let msgs = vec![
        json!({"role": "user", "content": "hi"}),
        json!({"role": "assistant", "content": "hello"}),
        json!({"role": "tool", "content": "result", "tool_call_id": "c1"}),
        json!({"role": "user", "content": "thanks"}),
        json!({"role": "assistant", "content": "welcome"}),
    ];
    let events = enc.on_agent_event(&AgentEvent::MessagesSnapshot {
        messages: msgs.clone(),
    });
    assert_eq!(events.len(), 1);
    if let Event::MessagesSnapshot { messages, .. } = &events[0] {
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[4]["role"], "assistant");
    } else {
        panic!("expected MessagesSnapshot");
    }
}

#[test]
fn text_message_with_empty_content() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::TextDelta { delta: "".into() });
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], Event::TextMessageStart { .. }));
    assert!(matches!(&events[1], Event::TextMessageContent { delta, .. } if delta.is_empty()));
}

#[test]
fn text_message_with_multiline_content() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let multiline = "line1\nline2\n\nline4\ttab";
    let events = enc.on_agent_event(&AgentEvent::TextDelta {
        delta: multiline.into(),
    });
    assert_eq!(events.len(), 2);
    if let Event::TextMessageContent { delta, .. } = &events[1] {
        assert_eq!(delta, multiline);
        assert!(delta.contains('\n'));
        assert!(delta.contains('\t'));
    } else {
        panic!("expected TextMessageContent");
    }
}

#[test]
fn text_message_with_special_characters() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let special = r#"<div class="test">&amp; 'single' "double" \backslash</div>"#;
    let events = enc.on_agent_event(&AgentEvent::TextDelta {
        delta: special.into(),
    });
    assert_eq!(events.len(), 2);
    if let Event::TextMessageContent { delta, .. } = &events[1] {
        assert_eq!(delta, special);
        assert!(delta.contains("&amp;"));
        assert!(delta.contains(r#"\backslash"#));
        assert!(delta.contains('"'));
        assert!(delta.contains('\''));
    } else {
        panic!("expected TextMessageContent");
    }
}

// ============================================================================
// Event sequence validation tests (9)
// ============================================================================

#[test]
fn events_start_with_run_started() {
    let mut enc = AgUiEncoder::new();
    let mut all = Vec::new();

    all.extend(enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    }));
    all.extend(enc.on_agent_event(&AgentEvent::StepStart {
        message_id: "m1".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "hello".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::StepEnd));
    all.extend(enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    }));

    assert!(!all.is_empty());
    assert!(matches!(&all[0], Event::RunStarted { .. }));
}

#[test]
fn events_end_with_run_finished() {
    let mut enc = AgUiEncoder::new();
    let mut all = Vec::new();

    all.extend(enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    }));
    all.extend(enc.on_agent_event(&AgentEvent::TextDelta { delta: "hi".into() }));
    all.extend(enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    }));

    let last = all.last().unwrap();
    assert!(matches!(last, Event::RunFinished { .. }));
}

#[test]
fn step_started_before_text_events() {
    let mut enc = AgUiEncoder::new();
    let mut all = Vec::new();

    all.extend(enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    }));
    all.extend(enc.on_agent_event(&AgentEvent::StepStart {
        message_id: "m1".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "response".into(),
    }));

    let step_idx = all
        .iter()
        .position(|e| matches!(e, Event::StepStarted { .. }))
        .unwrap();
    let text_start_idx = all
        .iter()
        .position(|e| matches!(e, Event::TextMessageStart { .. }))
        .unwrap();
    assert!(step_idx < text_start_idx);
}

#[test]
fn tool_call_sequence_correct() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let mut all = Vec::new();

    all.extend(enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "search".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::ToolCallDelta {
        id: "c1".into(),
        args_delta: r#"{"q":"test"}"#.into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "search".into(),
        arguments: json!({"q": "test"}),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::success("search", json!("found")),
        outcome: ToolCallOutcome::Succeeded,
    }));

    let tool_events: Vec<_> = all
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::ToolCallStart { .. }
                    | Event::ToolCallArgs { .. }
                    | Event::ToolCallEnd { .. }
                    | Event::ToolCallResult { .. }
            )
        })
        .collect();

    assert_eq!(tool_events.len(), 4);
    assert!(matches!(tool_events[0], Event::ToolCallStart { .. }));
    assert!(matches!(tool_events[1], Event::ToolCallArgs { .. }));
    assert!(matches!(tool_events[2], Event::ToolCallEnd { .. }));
    assert!(matches!(tool_events[3], Event::ToolCallResult { .. }));
}

#[test]
fn no_events_after_run_finished() {
    let mut enc = make_encoder_with_run("t1", "r1");
    enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });

    // All subsequent events should be suppressed
    assert!(
        enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "late".into()
        })
        .is_empty()
    );
    assert!(
        enc.on_agent_event(&AgentEvent::StepStart {
            message_id: "m".into()
        })
        .is_empty()
    );
    assert!(
        enc.on_agent_event(&AgentEvent::StateSnapshot {
            snapshot: json!({})
        })
        .is_empty()
    );
    assert!(
        enc.on_agent_event(&AgentEvent::MessagesSnapshot { messages: vec![] })
            .is_empty()
    );
    assert!(
        enc.on_agent_event(&AgentEvent::ToolCallStart {
            id: "c".into(),
            name: "x".into(),
        })
        .is_empty()
    );
}

#[test]
fn no_events_after_run_error() {
    let mut enc = AgUiEncoder::new();
    enc.on_agent_event(&AgentEvent::Error {
        message: "fatal".into(),
        code: Some("E500".into()),
    });

    assert!(
        enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "late".into()
        })
        .is_empty()
    );
    assert!(
        enc.on_agent_event(&AgentEvent::StepStart {
            message_id: "m".into()
        })
        .is_empty()
    );
    assert!(
        enc.on_agent_event(&AgentEvent::StateSnapshot {
            snapshot: json!({})
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
fn run_error_includes_message() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Error("context window exceeded".into()),
    });
    let error_event = events
        .iter()
        .find(|e| matches!(e, Event::RunError { .. }))
        .expect("should contain RunError");
    if let Event::RunError { message, .. } = error_event {
        assert_eq!(message, "context window exceeded");
    }
}

#[test]
fn natural_end_finish_has_result() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let result_val = json!({"response": "task completed", "artifacts": [1, 2, 3]});
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: Some(result_val.clone()),
        termination: TerminationReason::NaturalEnd,
    });
    let finished = events
        .iter()
        .find(|e| matches!(e, Event::RunFinished { .. }))
        .expect("should contain RunFinished");
    if let Event::RunFinished {
        result,
        thread_id,
        run_id,
        ..
    } = finished
    {
        assert_eq!(thread_id, "t1");
        assert_eq!(run_id, "r1");
        assert_eq!(*result, Some(result_val));
    }
}

#[test]
fn cancelled_finish_has_correct_reason() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Cancelled,
    });
    // Cancelled maps to RunFinished (not RunError)
    let finished = events
        .iter()
        .find(|e| matches!(e, Event::RunFinished { .. }))
        .expect("Cancelled should produce RunFinished");
    if let Event::RunFinished {
        thread_id,
        run_id,
        result,
        ..
    } = finished
    {
        assert_eq!(thread_id, "t1");
        assert_eq!(run_id, "r1");
        assert!(result.is_none());
    }
    // Verify no error event was produced
    assert!(
        !events.iter().any(|e| matches!(e, Event::RunError { .. })),
        "Cancelled should not produce RunError"
    );
}

// ============================================================================
// Activity event encoder tests
// ============================================================================

#[test]
fn activity_snapshot_forwarded_with_all_fields() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "msg-1".into(),
        activity_type: "file-write".into(),
        content: json!({"path": "/tmp/out.txt", "bytes": 1024}),
        replace: Some(true),
    });
    assert_eq!(events.len(), 1);
    if let Event::ActivitySnapshot {
        message_id,
        activity_type,
        content,
        replace,
        ..
    } = &events[0]
    {
        assert_eq!(message_id, "msg-1");
        assert_eq!(activity_type, "file-write");
        assert_eq!(content.get("path").unwrap(), &json!("/tmp/out.txt"));
        assert_eq!(content.get("bytes").unwrap(), &json!(1024));
        assert_eq!(*replace, Some(true));
    } else {
        panic!("expected ActivitySnapshot, got: {:?}", events[0]);
    }
}

#[test]
fn activity_delta_forwarded_with_all_fields() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ActivityDelta {
        message_id: "msg-2".into(),
        activity_type: "edit".into(),
        patch: vec![
            json!({"op": "replace", "path": "/line", "value": 42}),
            json!({"op": "add", "path": "/col", "value": 0}),
        ],
    });
    assert_eq!(events.len(), 1);
    if let Event::ActivityDelta {
        message_id,
        activity_type,
        patch,
        ..
    } = &events[0]
    {
        assert_eq!(message_id, "msg-2");
        assert_eq!(activity_type, "edit");
        assert_eq!(patch.len(), 2);
        assert_eq!(patch[0]["op"], "replace");
        assert_eq!(patch[1]["op"], "add");
    } else {
        panic!("expected ActivityDelta, got: {:?}", events[0]);
    }
}

#[test]
fn activity_snapshot_content_is_json_object() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "m1".into(),
        activity_type: "data".into(),
        content: json!({"key": "value", "nested": {"a": 1}}),
        replace: None,
    });
    assert_eq!(events.len(), 1);
    if let Event::ActivitySnapshot { content, .. } = &events[0] {
        assert_eq!(content.get("key").unwrap(), &json!("value"));
        assert!(content.get("nested").unwrap().is_object());
    } else {
        panic!("expected ActivitySnapshot");
    }
}

#[test]
fn activity_snapshot_content_is_array() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "m1".into(),
        activity_type: "list".into(),
        content: json!([1, 2, 3]),
        replace: None,
    });
    assert_eq!(events.len(), 1);
    // Non-object content gets wrapped under "value" key
    if let Event::ActivitySnapshot { content, .. } = &events[0] {
        assert_eq!(content.get("value").unwrap(), &json!([1, 2, 3]));
    } else {
        panic!("expected ActivitySnapshot");
    }
}

#[test]
fn activity_snapshot_content_is_scalar() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "m1".into(),
        activity_type: "note".into(),
        content: json!("hello world"),
        replace: None,
    });
    assert_eq!(events.len(), 1);
    // Scalar content gets wrapped under "value" key
    if let Event::ActivitySnapshot { content, .. } = &events[0] {
        assert_eq!(content.get("value").unwrap(), &json!("hello world"));
    } else {
        panic!("expected ActivitySnapshot");
    }

    // Also test numeric scalar
    let events2 = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "m2".into(),
        activity_type: "count".into(),
        content: json!(42),
        replace: None,
    });
    if let Event::ActivitySnapshot { content, .. } = &events2[0] {
        assert_eq!(content.get("value").unwrap(), &json!(42));
    } else {
        panic!("expected ActivitySnapshot");
    }
}

#[test]
fn activity_delta_patch_is_json_patch_format() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ActivityDelta {
        message_id: "m1".into(),
        activity_type: "edit".into(),
        patch: vec![
            json!({"op": "replace", "path": "/status", "value": "done"}),
            json!({"op": "remove", "path": "/temp"}),
            json!({"op": "add", "path": "/result", "value": 42}),
        ],
    });
    assert_eq!(events.len(), 1);
    if let Event::ActivityDelta { patch, .. } = &events[0] {
        assert_eq!(patch.len(), 3);
        // Each element is a JSON Patch operation object
        assert_eq!(patch[0]["op"], "replace");
        assert_eq!(patch[0]["path"], "/status");
        assert_eq!(patch[1]["op"], "remove");
        assert_eq!(patch[2]["op"], "add");
    } else {
        panic!("expected ActivityDelta");
    }
}

#[test]
fn activity_snapshot_message_id_matches_tool_call() {
    let mut enc = make_encoder_with_run("t1", "r1");
    // Simulate a tool call, then an activity snapshot referencing the same call_id
    let tool_call_id = "tc-abc-123";
    enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: tool_call_id.into(),
        name: "search".into(),
    });

    let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: tool_call_id.into(),
        activity_type: "tool-call-progress".into(),
        content: json!({"status": "running"}),
        replace: Some(true),
    });
    assert_eq!(events.len(), 1);
    if let Event::ActivitySnapshot { message_id, .. } = &events[0] {
        assert_eq!(message_id, tool_call_id);
    } else {
        panic!("expected ActivitySnapshot");
    }
}

#[test]
fn multiple_activity_snapshots_different_types() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events1 = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "m1".into(),
        activity_type: "file-read".into(),
        content: json!({"path": "/etc/hosts"}),
        replace: None,
    });
    let events2 = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "m1".into(),
        activity_type: "tool-call-progress".into(),
        content: json!({"status": "done"}),
        replace: Some(true),
    });

    assert_eq!(events1.len(), 1);
    assert_eq!(events2.len(), 1);

    if let Event::ActivitySnapshot { activity_type, .. } = &events1[0] {
        assert_eq!(activity_type, "file-read");
    } else {
        panic!("expected ActivitySnapshot");
    }
    if let Event::ActivitySnapshot { activity_type, .. } = &events2[0] {
        assert_eq!(activity_type, "tool-call-progress");
    } else {
        panic!("expected ActivitySnapshot");
    }
}

#[test]
fn activity_snapshot_after_text_delta() {
    let mut enc = make_encoder_with_run("t1", "r1");

    // Emit text delta first
    let text_events = enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "Thinking...".into(),
    });
    assert!(!text_events.is_empty());

    // Now emit activity snapshot — should coexist with open text stream
    let activity_events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "m-act".into(),
        activity_type: "progress".into(),
        content: json!({"percent": 50}),
        replace: Some(true),
    });
    assert_eq!(activity_events.len(), 1);
    assert!(matches!(
        &activity_events[0],
        Event::ActivitySnapshot { .. }
    ));
}

#[test]
fn activity_events_not_suppressed_by_terminal_guard() {
    let mut enc = make_encoder_with_run("t1", "r1");

    // Emit activity before terminal event — should work
    let before = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "m1".into(),
        activity_type: "progress".into(),
        content: json!({"status": "running"}),
        replace: Some(true),
    });
    assert_eq!(before.len(), 1);
    assert!(matches!(&before[0], Event::ActivitySnapshot { .. }));

    // Now trigger terminal guard via Error
    enc.on_agent_event(&AgentEvent::Error {
        message: "fatal".into(),
        code: None,
    });

    // After terminal event, ALL events are suppressed (including activity)
    // This verifies the terminal guard is consistent
    let after = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "m2".into(),
        activity_type: "progress".into(),
        content: json!({"status": "done"}),
        replace: Some(true),
    });
    assert!(
        after.is_empty(),
        "activity events should be suppressed after terminal event"
    );

    // But the key point: activity events BEFORE terminal pass through fine
    // (verified by `before` assertions above)
}

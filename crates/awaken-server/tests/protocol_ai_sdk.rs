//! AI SDK v6 encoder contract tests — migrated from tirea-protocol-ai-sdk-v6.
//!
//! Validates event mapping, text block lifecycle, tool call handling,
//! finish reason mapping, reasoning blocks, and message ID propagation.

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::lifecycle::{StoppedReason, TerminationReason};
use awaken_contract::contract::suspension::ToolCallOutcome;
use awaken_contract::contract::tool::ToolResult;
use awaken_contract::contract::transport::Transcoder;
use awaken_server::protocols::ai_sdk_v6::encoder::AiSdkEncoder;
use awaken_server::protocols::ai_sdk_v6::types::UIStreamEvent;
use serde_json::json;

// ============================================================================
// Helper
// ============================================================================

fn make_encoder_with_run(thread_id: &str, run_id: &str) -> AiSdkEncoder {
    let mut enc = AiSdkEncoder::new();
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
    let mut enc = AiSdkEncoder::new();
    let events = enc.transcode(&AgentEvent::TextDelta { delta: "hi".into() });
    assert!(!events.is_empty());
}

// ============================================================================
// RunStart emits MessageStart + run-info
// ============================================================================

#[test]
fn run_start_emits_message_start_and_run_info() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "thread_1".into(),
        run_id: "run_12345678".into(),
        parent_run_id: None,
    });
    assert_eq!(events.len(), 2);
    assert!(
        matches!(&events[0], UIStreamEvent::MessageStart { message_id: Some(id), .. } if id == "run_12345678")
    );
    assert!(
        matches!(&events[1], UIStreamEvent::Data { data_type, .. } if data_type == "data-run-info")
    );
}

// ============================================================================
// Text block lifecycle
// ============================================================================

#[test]
fn text_delta_opens_text_block() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::TextDelta { delta: "hi".into() });
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], UIStreamEvent::TextStart { id, .. } if id == "txt_0"));
    assert!(matches!(&events[1], UIStreamEvent::TextDelta { delta, .. } if delta == "hi"));
}

#[test]
fn second_text_delta_reuses_open_block() {
    let mut enc = AiSdkEncoder::new();
    enc.on_agent_event(&AgentEvent::TextDelta { delta: "a".into() });
    let events = enc.on_agent_event(&AgentEvent::TextDelta { delta: "b".into() });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], UIStreamEvent::TextDelta { delta, .. } if delta == "b"));
}

#[test]
fn text_counter_increments_after_close() {
    let mut enc = AiSdkEncoder::new();
    enc.on_agent_event(&AgentEvent::TextDelta { delta: "a".into() });
    enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "t".into(),
    });
    let events = enc.on_agent_event(&AgentEvent::TextDelta { delta: "b".into() });
    assert!(matches!(&events[0], UIStreamEvent::TextStart { id, .. } if id == "txt_1"));
}

// ============================================================================
// Tool call handling
// ============================================================================

#[test]
fn tool_call_start_closes_text_block() {
    let mut enc = AiSdkEncoder::new();
    enc.on_agent_event(&AgentEvent::TextDelta { delta: "hi".into() });
    let events = enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "search".into(),
    });
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], UIStreamEvent::TextEnd { .. }));
    assert!(matches!(&events[1], UIStreamEvent::ToolInputStart { .. }));
}

#[test]
fn tool_call_delta_maps_to_tool_input_delta() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallDelta {
        id: "c1".into(),
        args_delta: r#"{"q":"#.into(),
    });
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], UIStreamEvent::ToolInputDelta { tool_call_id, .. } if tool_call_id == "c1")
    );
}

#[test]
fn tool_call_ready_maps_to_tool_input_available() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "search".into(),
        arguments: json!({"q": "rust"}),
    });
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], UIStreamEvent::ToolInputAvailable { tool_call_id, .. } if tool_call_id == "c1")
    );
}

#[test]
fn tool_call_done_success_maps_to_output_available() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::success("search", json!({"items": [1]})),
        outcome: ToolCallOutcome::Succeeded,
    });
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], UIStreamEvent::ToolOutputAvailable { tool_call_id, .. } if tool_call_id == "c1")
    );
}

#[test]
fn tool_call_done_error_maps_to_output_error() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::error("search", "not found"),
        outcome: ToolCallOutcome::Failed,
    });
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], UIStreamEvent::ToolOutputError { error_text, .. } if error_text == "not found")
    );
}

#[test]
fn tool_call_done_pending_maps_to_approval_request() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::suspended("confirm", "needs approval"),
        outcome: ToolCallOutcome::Suspended,
    });
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], UIStreamEvent::ToolApprovalRequest { tool_call_id, .. } if tool_call_id == "c1")
    );
}

// ============================================================================
// ToolCallResumed handling
// ============================================================================

#[test]
fn tool_call_resumed_approved_maps_to_output_available() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
        target_id: "c1".into(),
        result: json!({"approved": true}),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        UIStreamEvent::ToolOutputAvailable { .. }
    ));
}

#[test]
fn tool_call_resumed_denied_maps_to_output_denied() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
        target_id: "c1".into(),
        result: json!({"approved": false}),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], UIStreamEvent::ToolOutputDenied { .. }));
}

#[test]
fn tool_call_resumed_error_maps_to_output_error() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
        target_id: "c1".into(),
        result: json!({"error": "validation failed"}),
    });
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], UIStreamEvent::ToolOutputError { error_text, .. }
        if error_text == "validation failed")
    );
}

// ============================================================================
// Finish reason mapping
// ============================================================================

#[test]
fn natural_end_maps_to_stop_reason() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    match events.last().unwrap() {
        UIStreamEvent::Finish { finish_reason, .. } => {
            assert_eq!(finish_reason.as_deref(), Some("stop"));
        }
        other => panic!("expected Finish, got: {other:?}"),
    }
}

#[test]
fn suspended_maps_to_tool_calls_reason() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Suspended,
    });
    match events.last().unwrap() {
        UIStreamEvent::Finish { finish_reason, .. } => {
            assert_eq!(finish_reason.as_deref(), Some("tool-calls"));
        }
        other => panic!("expected Finish, got: {other:?}"),
    }
}

#[test]
fn error_termination_maps_to_error_reason() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Error("boom".into()),
    });
    match events.last().unwrap() {
        UIStreamEvent::Finish { finish_reason, .. } => {
            assert_eq!(finish_reason.as_deref(), Some("error"));
        }
        other => panic!("expected Finish, got: {other:?}"),
    }
}

#[test]
fn blocked_maps_to_content_filter_reason() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Blocked("unsafe".into()),
    });
    match events.last().unwrap() {
        UIStreamEvent::Finish { finish_reason, .. } => {
            assert_eq!(finish_reason.as_deref(), Some("content-filter"));
        }
        other => panic!("expected Finish, got: {other:?}"),
    }
}

#[test]
fn cancelled_maps_to_stop_reason() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Cancelled,
    });
    match events.last().unwrap() {
        UIStreamEvent::Finish { finish_reason, .. } => {
            assert_eq!(finish_reason.as_deref(), Some("stop"));
        }
        other => panic!("expected Finish, got: {other:?}"),
    }
}

#[test]
fn stopped_max_rounds_maps_to_stop_reason() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Stopped(StoppedReason::new("max_rounds_reached")),
    });
    match events.last().unwrap() {
        UIStreamEvent::Finish { finish_reason, .. } => {
            assert_eq!(finish_reason.as_deref(), Some("stop"));
        }
        other => panic!("expected Finish, got: {other:?}"),
    }
}

// ============================================================================
// Terminal guard
// ============================================================================

#[test]
fn events_after_finish_are_suppressed() {
    let mut enc = AiSdkEncoder::new();
    enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    assert!(
        enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "ignored".into()
        })
        .is_empty()
    );
}

#[test]
fn events_after_error_are_suppressed() {
    let mut enc = AiSdkEncoder::new();
    enc.on_agent_event(&AgentEvent::Error {
        message: "fatal".into(),
        code: Some("E001".into()),
    });
    assert!(
        enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "ignored".into()
        })
        .is_empty()
    );
}

// ============================================================================
// Run finish closes open text
// ============================================================================

#[test]
fn run_finish_closes_text_and_emits_finish() {
    let mut enc = AiSdkEncoder::new();
    enc.on_agent_event(&AgentEvent::TextDelta { delta: "hi".into() });
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    assert!(events.len() >= 2);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, UIStreamEvent::TextEnd { .. }))
    );
    assert!(matches!(
        events.last().unwrap(),
        UIStreamEvent::Finish { .. }
    ));
}

// ============================================================================
// Reasoning events
// ============================================================================

#[test]
fn reasoning_delta_opens_reasoning_block() {
    let mut enc = AiSdkEncoder::new();
    enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "msg1".into(),
        parent_run_id: None,
    });
    let events = enc.on_agent_event(&AgentEvent::ReasoningDelta {
        delta: "thinking".into(),
    });
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], UIStreamEvent::ReasoningStart { .. }));
    assert!(
        matches!(&events[1], UIStreamEvent::ReasoningDelta { delta, .. } if delta == "thinking")
    );
}

#[test]
fn second_reasoning_delta_reuses_open_block() {
    let mut enc = make_encoder_with_run("t1", "r1");
    enc.on_agent_event(&AgentEvent::ReasoningDelta { delta: "a".into() });
    let events = enc.on_agent_event(&AgentEvent::ReasoningDelta { delta: "b".into() });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], UIStreamEvent::ReasoningDelta { delta, .. } if delta == "b"));
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
            .any(|e| matches!(e, UIStreamEvent::ReasoningEnd { .. }))
    );
    assert!(matches!(
        events.last().unwrap(),
        UIStreamEvent::Finish { .. }
    ));
}

#[test]
fn reasoning_encrypted_value_emits_data_event() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ReasoningEncryptedValue {
        encrypted_value: "opaque-token".into(),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], UIStreamEvent::Data { data_type, .. }
        if data_type == "data-reasoning-encrypted"));
}

// ============================================================================
// Step events
// ============================================================================

#[test]
fn step_events_pass_through() {
    let mut enc = AiSdkEncoder::new();
    let start = enc.on_agent_event(&AgentEvent::StepStart {
        message_id: "m1".into(),
    });
    assert_eq!(start.len(), 1);
    assert!(matches!(&start[0], UIStreamEvent::StartStep));

    let end = enc.on_agent_event(&AgentEvent::StepEnd);
    assert_eq!(end.len(), 1);
    assert!(matches!(&end[0], UIStreamEvent::FinishStep));
}

// ============================================================================
// Data events (state, messages, activity, inference)
// ============================================================================

#[test]
fn state_snapshot_emits_data_event() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({"key": "value"}),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], UIStreamEvent::Data { data_type, .. }
        if data_type == "data-state-snapshot"));
}

#[test]
fn state_delta_emits_data_event() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::StateDelta {
        delta: vec![json!({"op": "add", "path": "/x", "value": 1})],
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], UIStreamEvent::Data { data_type, .. }
        if data_type == "data-state-delta"));
}

#[test]
fn messages_snapshot_emits_data_event() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::MessagesSnapshot {
        messages: vec![json!({"role": "user", "content": "hi"})],
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], UIStreamEvent::Data { data_type, .. }
        if data_type == "data-messages-snapshot"));
}

#[test]
fn activity_snapshot_emits_data_event() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "m1".into(),
        activity_type: "thinking".into(),
        content: json!({"text": "processing"}),
        replace: Some(true),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], UIStreamEvent::Data { data_type, .. }
        if data_type == "data-activity-snapshot"));
}

#[test]
fn activity_delta_emits_data_event() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ActivityDelta {
        message_id: "m1".into(),
        activity_type: "progress".into(),
        patch: vec![json!({"op": "replace", "path": "/p", "value": 50})],
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], UIStreamEvent::Data { data_type, .. }
        if data_type == "data-activity-delta"));
}

#[test]
fn inference_complete_emits_data_event() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::InferenceComplete {
        model: "gpt-4o".into(),
        usage: None,
        duration_ms: 1234,
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], UIStreamEvent::Data { data_type, .. }
        if data_type == "data-inference-complete"));
}

// ============================================================================
// Message ID propagation
// ============================================================================

#[test]
fn encoder_adopts_first_step_start_message_id() {
    let mut enc = AiSdkEncoder::new();
    enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "run_12345678".into(),
        parent_run_id: None,
    });
    let initial_id = enc.message_id().to_string();
    assert!(!initial_id.is_empty(), "RunStart must set a message_id");

    let step_msg_id = "pre-gen-assistant-uuid";
    enc.on_agent_event(&AgentEvent::StepStart {
        message_id: step_msg_id.into(),
    });
    assert_eq!(
        enc.message_id(),
        step_msg_id,
        "AiSdkEncoder must adopt StepStart.message_id"
    );

    // Second StepStart should not override
    enc.on_agent_event(&AgentEvent::StepStart {
        message_id: "second-step-id".into(),
    });
    assert_eq!(
        enc.message_id(),
        step_msg_id,
        "AiSdkEncoder must keep the first StepStart.message_id"
    );
}

// ============================================================================
// Error event
// ============================================================================

#[test]
fn error_event_emits_error() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::Error {
        message: "something failed".into(),
        code: Some("E001".into()),
    });
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], UIStreamEvent::Error { error_text } if error_text == "something failed")
    );
}

// ============================================================================
// Full lifecycle: text → tool → finish
// ============================================================================

#[test]
fn full_lifecycle_text_tool_finish() {
    let mut enc = AiSdkEncoder::new();

    // RunStart
    let ev = enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    });
    assert_eq!(ev.len(), 2);

    // Text
    let ev = enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "hello".into(),
    });
    assert_eq!(ev.len(), 2);
    assert!(matches!(&ev[0], UIStreamEvent::TextStart { .. }));
    assert!(matches!(&ev[1], UIStreamEvent::TextDelta { delta, .. } if delta == "hello"));

    // Tool closes text
    let ev = enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "call_1".into(),
        name: "search".into(),
    });
    assert_eq!(ev.len(), 2);
    assert!(matches!(&ev[0], UIStreamEvent::TextEnd { .. }));
    assert!(matches!(&ev[1], UIStreamEvent::ToolInputStart { .. }));

    // Finish
    let ev = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    assert!(
        matches!(ev.last().unwrap(), UIStreamEvent::Finish { finish_reason: Some(r), .. } if r == "stop")
    );
}

// ============================================================================
// Serde roundtrips for UIStreamEvent types
// ============================================================================

#[test]
fn message_start_serde() {
    let event = UIStreamEvent::message_start("msg-1");
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"start\""));
    assert!(json.contains("\"messageId\":\"msg-1\""));
}

#[test]
fn text_delta_serde() {
    let event = UIStreamEvent::text_delta("txt_0", "hello");
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"text-delta\""));
    assert!(json.contains("\"delta\":\"hello\""));
}

#[test]
fn tool_input_start_serde() {
    let event = UIStreamEvent::tool_input_start("c1", "search");
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"tool-input-start\""));
    assert!(json.contains("\"toolCallId\":\"c1\""));
}

#[test]
fn finish_omits_none_fields() {
    let event = UIStreamEvent::finish();
    let json = serde_json::to_string(&event).unwrap();
    assert!(!json.contains("finishReason"));
}

#[test]
fn finish_with_reason_includes_field() {
    let event = UIStreamEvent::finish_with_reason("stop");
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"finishReason\":\"stop\""));
}

#[test]
fn data_event_prepends_data_prefix() {
    let event = UIStreamEvent::data("activity-snapshot", json!({"key": "val"}));
    match &event {
        UIStreamEvent::Data { data_type, .. } => assert_eq!(data_type, "data-activity-snapshot"),
        _ => panic!("expected Data variant"),
    }
}

#[test]
fn data_event_preserves_existing_prefix() {
    let event = UIStreamEvent::data("data-custom", json!(null));
    match &event {
        UIStreamEvent::Data { data_type, .. } => assert_eq!(data_type, "data-custom"),
        _ => panic!("expected Data variant"),
    }
}

#[test]
fn error_event_serde() {
    let event = UIStreamEvent::error("something failed");
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"errorText\":\"something failed\""));
}

#[test]
fn step_events_serde() {
    let start = UIStreamEvent::start_step();
    let end = UIStreamEvent::finish_step();
    let s_json = serde_json::to_string(&start).unwrap();
    let e_json = serde_json::to_string(&end).unwrap();
    assert!(s_json.contains("\"type\":\"start-step\""));
    assert!(e_json.contains("\"type\":\"finish-step\""));
}

#[test]
fn reasoning_events_serde() {
    let start = UIStreamEvent::reasoning_start("r1");
    let delta = UIStreamEvent::reasoning_delta("r1", "thinking...");
    let end = UIStreamEvent::reasoning_end("r1");
    assert!(
        serde_json::to_string(&start)
            .unwrap()
            .contains("reasoning-start")
    );
    assert!(
        serde_json::to_string(&delta)
            .unwrap()
            .contains("thinking...")
    );
    assert!(
        serde_json::to_string(&end)
            .unwrap()
            .contains("reasoning-end")
    );
}

#[test]
fn tool_output_events_serde() {
    let available = UIStreamEvent::tool_output_available("c1", json!(42));
    let error = UIStreamEvent::tool_output_error("c1", "fail");
    let denied = UIStreamEvent::tool_output_denied("c1");
    assert!(
        serde_json::to_string(&available)
            .unwrap()
            .contains("tool-output-available")
    );
    assert!(
        serde_json::to_string(&error)
            .unwrap()
            .contains("tool-output-error")
    );
    assert!(
        serde_json::to_string(&denied)
            .unwrap()
            .contains("tool-output-denied")
    );
}

#[test]
fn tool_approval_request_serde() {
    let event = UIStreamEvent::tool_approval_request("a1", "c1");
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"approvalId\":\"a1\""));
    assert!(json.contains("\"toolCallId\":\"c1\""));
}

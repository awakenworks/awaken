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
    // Pending now emits nothing — frontend handles input via its own UI
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::suspended("confirm", "needs approval"),
        outcome: ToolCallOutcome::Suspended,
    });
    assert!(events.is_empty());
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
fn suspended_emits_finish_tool_calls() {
    // Suspended RunFinish emits finish(tool-calls) and marks guard
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Suspended,
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, UIStreamEvent::Finish { finish_reason, .. }
                if finish_reason.as_deref() == Some("tool-calls")))
    );
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

// ============================================================================
// Tool Execution (10)
// ============================================================================

// 1.
#[test]
fn tool_call_start_has_id_and_name() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "call_abc".into(),
        name: "web_search".into(),
    });
    // Should emit ToolInputStart with matching id and name
    let tool_start = events
        .iter()
        .find(|e| matches!(e, UIStreamEvent::ToolInputStart { .. }))
        .expect("expected ToolInputStart event");
    match tool_start {
        UIStreamEvent::ToolInputStart {
            tool_call_id,
            tool_name,
            ..
        } => {
            assert_eq!(tool_call_id, "call_abc");
            assert_eq!(tool_name, "web_search");
        }
        _ => unreachable!(),
    }
}

// 2.
#[test]
fn tool_call_args_encodes_arguments() {
    let mut enc = make_encoder_with_run("t1", "r1");
    enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "search".into(),
    });
    let events = enc.on_agent_event(&AgentEvent::ToolCallDelta {
        id: "c1".into(),
        args_delta: r#"{"query": "rust"}"#.into(),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::ToolInputDelta {
            tool_call_id,
            input_text_delta,
        } => {
            assert_eq!(tool_call_id, "c1");
            assert_eq!(input_text_delta, r#"{"query": "rust"}"#);
        }
        other => panic!("expected ToolInputDelta, got: {other:?}"),
    }
}

// 3.
#[test]
fn tool_call_end_emits_after_ready() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let mut all = Vec::new();
    all.extend(enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "search".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::ToolCallDelta {
        id: "c1".into(),
        args_delta: r#"{"q":"rust"}"#.into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "search".into(),
        arguments: json!({"q": "rust"}),
    }));

    // ToolInputAvailable should come after ToolInputStart and ToolInputDelta
    let start_idx = all
        .iter()
        .position(|e| matches!(e, UIStreamEvent::ToolInputStart { .. }))
        .unwrap();
    let delta_idx = all
        .iter()
        .position(|e| matches!(e, UIStreamEvent::ToolInputDelta { .. }))
        .unwrap();
    let available_idx = all
        .iter()
        .position(|e| matches!(e, UIStreamEvent::ToolInputAvailable { .. }))
        .unwrap();
    assert!(start_idx < delta_idx);
    assert!(delta_idx < available_idx);
}

// 4.
#[test]
fn tool_call_result_has_correct_payload() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let result_data = json!({"results": [{"title": "Rust", "url": "https://rust-lang.org"}]});
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::success("search", result_data.clone()),
        outcome: ToolCallOutcome::Succeeded,
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::ToolOutputAvailable {
            tool_call_id,
            output,
            ..
        } => {
            assert_eq!(tool_call_id, "c1");
            assert_eq!(*output, result_data);
        }
        other => panic!("expected ToolOutputAvailable, got: {other:?}"),
    }
}

// 5.
#[test]
fn multiple_tool_calls_correct_sequence() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let mut all = Vec::new();

    // Tool 1
    all.extend(enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "search".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "search".into(),
        arguments: json!({"q": "rust"}),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::success("search", json!({"found": true})),
        outcome: ToolCallOutcome::Succeeded,
    }));

    // Tool 2
    all.extend(enc.on_agent_event(&AgentEvent::ToolCallStart {
        id: "c2".into(),
        name: "fetch".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "c2".into(),
        name: "fetch".into(),
        arguments: json!({"url": "https://example.com"}),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c2".into(),
        message_id: "m2".into(),
        result: ToolResult::success("fetch", json!({"status": 200})),
        outcome: ToolCallOutcome::Succeeded,
    }));

    // Verify both tool starts present with correct ids
    let starts: Vec<_> = all
        .iter()
        .filter_map(|e| match e {
            UIStreamEvent::ToolInputStart {
                tool_call_id,
                tool_name,
                ..
            } => Some((tool_call_id.as_str(), tool_name.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(starts, vec![("c1", "search"), ("c2", "fetch")]);

    // Verify both outputs present
    let outputs: Vec<_> = all
        .iter()
        .filter_map(|e| match e {
            UIStreamEvent::ToolOutputAvailable { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(outputs, vec!["c1", "c2"]);
}

// 6.
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
    match &events[0] {
        UIStreamEvent::ToolOutputError {
            tool_call_id,
            error_text,
            ..
        } => {
            assert_eq!(tool_call_id, "c1");
            assert_eq!(error_text, "connection refused");
        }
        other => panic!("expected ToolOutputError, got: {other:?}"),
    }
}

// 7.
#[test]
fn unknown_tool_error_does_not_crash() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "c_unknown".into(),
        message_id: "m_unk".into(),
        result: ToolResult::error("nonexistent_tool", "tool not found"),
        outcome: ToolCallOutcome::Failed,
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::ToolOutputError {
            tool_call_id,
            error_text,
            ..
        } => {
            assert_eq!(tool_call_id, "c_unknown");
            assert!(error_text.contains("not found"));
        }
        other => panic!("expected ToolOutputError, got: {other:?}"),
    }
}

// 8.
#[test]
fn tool_call_with_empty_arguments() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "no_args_tool".into(),
        arguments: json!({}),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::ToolInputAvailable {
            tool_call_id,
            tool_name,
            input,
            ..
        } => {
            assert_eq!(tool_call_id, "c1");
            assert_eq!(tool_name, "no_args_tool");
            assert_eq!(*input, json!({}));
        }
        other => panic!("expected ToolInputAvailable, got: {other:?}"),
    }
}

// 9.
#[test]
fn tool_suspension_no_result_event() {
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
    // Suspended tool emits nothing — frontend handles input via its own UI
    assert!(events.is_empty());
    // No ToolOutputAvailable should be present
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, UIStreamEvent::ToolOutputAvailable { .. }))
    );
}

// 10.
#[test]
fn tool_result_contains_tool_call_id() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "call_xyz_789".into(),
        message_id: "m1".into(),
        result: ToolResult::success("search", json!("done")),
        outcome: ToolCallOutcome::Succeeded,
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::ToolOutputAvailable { tool_call_id, .. } => {
            assert_eq!(tool_call_id, "call_xyz_789");
        }
        other => panic!("expected ToolOutputAvailable, got: {other:?}"),
    }
}

// ============================================================================
// Text & Message (8)
// ============================================================================

// 11.
#[test]
fn text_delta_encodes_content() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "Hello, world!".into(),
    });
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], UIStreamEvent::TextStart { .. }));
    match &events[1] {
        UIStreamEvent::TextDelta { delta, .. } => {
            assert_eq!(delta, "Hello, world!");
        }
        other => panic!("expected TextDelta, got: {other:?}"),
    }
}

// 12.
#[test]
fn multiple_text_deltas_concatenated() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let mut all = Vec::new();
    all.extend(enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "Hello ".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "world".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::TextDelta { delta: "!".into() }));

    // Should have 1 TextStart + 3 TextDeltas
    let starts: Vec<_> = all
        .iter()
        .filter(|e| matches!(e, UIStreamEvent::TextStart { .. }))
        .collect();
    let deltas: Vec<_> = all
        .iter()
        .filter_map(|e| match e {
            UIStreamEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(deltas.len(), 3);
    assert_eq!(deltas.join(""), "Hello world!");
}

// 13.
#[test]
fn run_start_has_thread_and_run_id() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "thread_42".into(),
        run_id: "run_99".into(),
        parent_run_id: None,
    });
    assert_eq!(events.len(), 2);
    // First event: MessageStart with run_id as message_id
    assert!(
        matches!(&events[0], UIStreamEvent::MessageStart { message_id: Some(id), .. } if id == "run_99")
    );
    // Second event: Data with run-info containing both ids
    match &events[1] {
        UIStreamEvent::Data {
            data_type, data, ..
        } => {
            assert_eq!(data_type, "data-run-info");
            assert_eq!(data["runId"], "run_99");
            assert_eq!(data["threadId"], "thread_42");
        }
        other => panic!("expected Data run-info, got: {other:?}"),
    }
}

// 14.
#[test]
fn run_finish_with_result() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: Some(json!({"summary": "completed"})),
        termination: TerminationReason::NaturalEnd,
    });
    assert!(matches!(
        events.last().unwrap(),
        UIStreamEvent::Finish {
            finish_reason: Some(r),
            ..
        } if r == "stop"
    ));
}

// 15.
#[test]
fn run_finish_cancelled_reason() {
    let mut enc = make_encoder_with_run("t1", "r1");
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

// 16.
#[test]
fn run_error_with_message() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::Error {
        message: "provider timeout".into(),
        code: Some("TIMEOUT".into()),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::Error { error_text } => {
            assert_eq!(error_text, "provider timeout");
        }
        other => panic!("expected Error, got: {other:?}"),
    }
}

// 17.
#[test]
fn empty_text_delta_still_emits() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::TextDelta { delta: "".into() });
    // Even empty delta should emit TextStart + TextDelta
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], UIStreamEvent::TextStart { .. }));
    assert!(matches!(&events[1], UIStreamEvent::TextDelta { delta, .. } if delta.is_empty()));
}

// 18.
#[test]
fn unicode_emoji_preserved() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let text = "Hello, \u{4e16}\u{754c}! \u{1f30d}\u{1f680}\u{2764}\u{fe0f}";
    let events = enc.on_agent_event(&AgentEvent::TextDelta { delta: text.into() });
    assert_eq!(events.len(), 2);
    match &events[1] {
        UIStreamEvent::TextDelta { delta, .. } => {
            assert_eq!(delta, text);
            assert!(delta.contains('\u{1f30d}')); // globe emoji
            assert!(delta.contains('\u{1f680}')); // rocket emoji
        }
        other => panic!("expected TextDelta, got: {other:?}"),
    }
}

// ============================================================================
// State & Activity (8)
// ============================================================================

// 19.
#[test]
fn state_snapshot_forwarded() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let state = json!({"counter": 42, "active": true});
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: state.clone(),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::Data {
            data_type, data, ..
        } => {
            assert_eq!(data_type, "data-state-snapshot");
            assert_eq!(*data, state);
        }
        other => panic!("expected Data state-snapshot, got: {other:?}"),
    }
}

// 20.
#[test]
fn state_snapshot_nested_json() {
    let mut enc = make_encoder_with_run("t1", "r1");
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
    match &events[0] {
        UIStreamEvent::Data {
            data_type, data, ..
        } => {
            assert_eq!(data_type, "data-state-snapshot");
            assert_eq!(*data, state);
            assert_eq!(data["user"]["profile"]["name"], "Alice");
            assert_eq!(
                data["user"]["profile"]["preferences"]["notifications"]["email"],
                true
            );
        }
        other => panic!("expected Data state-snapshot, got: {other:?}"),
    }
}

// 21.
#[test]
fn state_snapshot_empty_object() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({}),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::Data {
            data_type, data, ..
        } => {
            assert_eq!(data_type, "data-state-snapshot");
            assert_eq!(*data, json!({}));
            assert!(data.as_object().unwrap().is_empty());
        }
        other => panic!("expected Data state-snapshot, got: {other:?}"),
    }
}

// 22.
#[test]
fn activity_snapshot_forwarded() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "act_1".into(),
        activity_type: "search_progress".into(),
        content: json!({"found": 5, "total": 100}),
        replace: Some(false),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::Data {
            data_type, data, ..
        } => {
            assert_eq!(data_type, "data-activity-snapshot");
            assert_eq!(data["messageId"], "act_1");
            assert_eq!(data["activityType"], "search_progress");
            assert_eq!(data["content"]["found"], 5);
            assert_eq!(data["replace"], false);
        }
        other => panic!("expected Data activity-snapshot, got: {other:?}"),
    }
}

// 23.
#[test]
fn activity_delta_forwarded() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ActivityDelta {
        message_id: "act_1".into(),
        activity_type: "progress".into(),
        patch: vec![json!({"op": "replace", "path": "/percent", "value": 75})],
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::Data {
            data_type, data, ..
        } => {
            assert_eq!(data_type, "data-activity-delta");
            assert_eq!(data["messageId"], "act_1");
            assert_eq!(data["activityType"], "progress");
            assert_eq!(data["patch"][0]["op"], "replace");
            assert_eq!(data["patch"][0]["value"], 75);
        }
        other => panic!("expected Data activity-delta, got: {other:?}"),
    }
}

// 24.
#[test]
fn activity_snapshot_with_replace_flag() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
        message_id: "act_2".into(),
        activity_type: "status".into(),
        content: json!({"text": "processing"}),
        replace: Some(true),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::Data {
            data_type, data, ..
        } => {
            assert_eq!(data_type, "data-activity-snapshot");
            assert_eq!(data["replace"], true);
        }
        other => panic!("expected Data activity-snapshot, got: {other:?}"),
    }
}

// 25.
#[test]
fn messages_snapshot_forwarded() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let msgs = vec![
        json!({"role": "user", "content": "hello"}),
        json!({"role": "assistant", "content": "hi there"}),
    ];
    let events = enc.on_agent_event(&AgentEvent::MessagesSnapshot {
        messages: msgs.clone(),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::Data {
            data_type, data, ..
        } => {
            assert_eq!(data_type, "data-messages-snapshot");
            let arr = data.as_array().unwrap();
            assert_eq!(arr.len(), 2);
            assert_eq!(arr[0]["role"], "user");
            assert_eq!(arr[1]["content"], "hi there");
        }
        other => panic!("expected Data messages-snapshot, got: {other:?}"),
    }
}

// 26.
#[test]
fn multiple_state_snapshots() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let ev1 = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({"version": 1}),
    });
    let ev2 = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({"version": 2}),
    });
    let ev3 = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({"version": 3}),
    });
    assert_eq!(ev1.len(), 1);
    assert_eq!(ev2.len(), 1);
    assert_eq!(ev3.len(), 1);
    // Each snapshot emitted independently
    match &ev1[0] {
        UIStreamEvent::Data { data, .. } => assert_eq!(data["version"], 1),
        _ => panic!("expected Data"),
    }
    match &ev2[0] {
        UIStreamEvent::Data { data, .. } => assert_eq!(data["version"], 2),
        _ => panic!("expected Data"),
    }
    match &ev3[0] {
        UIStreamEvent::Data { data, .. } => assert_eq!(data["version"], 3),
        _ => panic!("expected Data"),
    }
}

// ============================================================================
// Event Sequence (8)
// ============================================================================

// 27.
#[test]
fn events_start_with_run_started() {
    let mut enc = AiSdkEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    });
    assert!(
        matches!(&events[0], UIStreamEvent::MessageStart { .. }),
        "first event must be MessageStart"
    );
}

// 28.
#[test]
fn events_end_with_run_finished() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    assert!(
        matches!(events.last().unwrap(), UIStreamEvent::Finish { .. }),
        "last event must be Finish"
    );
}

// 29.
#[test]
fn step_boundaries_present() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let mut all = Vec::new();
    all.extend(enc.on_agent_event(&AgentEvent::StepStart {
        message_id: "m1".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::TextDelta {
        delta: "response".into(),
    }));
    all.extend(enc.on_agent_event(&AgentEvent::StepEnd));

    let start_idx = all
        .iter()
        .position(|e| matches!(e, UIStreamEvent::StartStep))
        .expect("StartStep must be present");
    let end_idx = all
        .iter()
        .position(|e| matches!(e, UIStreamEvent::FinishStep))
        .expect("FinishStep must be present");
    assert!(start_idx < end_idx, "StartStep must come before FinishStep");
}

// 30.
#[test]
fn no_events_after_finish() {
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
        enc.on_agent_event(&AgentEvent::ToolCallStart {
            id: "c1".into(),
            name: "t".into()
        })
        .is_empty()
    );
    assert!(
        enc.on_agent_event(&AgentEvent::StateSnapshot {
            snapshot: json!({})
        })
        .is_empty()
    );
}

// 31.
#[test]
fn no_events_after_error() {
    let mut enc = make_encoder_with_run("t1", "r1");
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
    assert!(
        enc.on_agent_event(&AgentEvent::StepStart {
            message_id: "m1".into()
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

// 32.
#[test]
fn inference_complete_handled() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::InferenceComplete {
        model: "claude-3-opus".into(),
        usage: None,
        duration_ms: 2500,
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::Data {
            data_type, data, ..
        } => {
            assert_eq!(data_type, "data-inference-complete");
            assert_eq!(data["model"], "claude-3-opus");
            assert_eq!(data["durationMs"], 2500);
        }
        other => panic!("expected Data inference-complete, got: {other:?}"),
    }
}

// 33.
#[test]
fn natural_end_produces_finish() {
    let mut enc = make_encoder_with_run("t1", "r1");
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

// 34.
#[test]
fn blocked_produces_finish() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Blocked("unsafe content".into()),
    });
    match events.last().unwrap() {
        UIStreamEvent::Finish { finish_reason, .. } => {
            assert_eq!(finish_reason.as_deref(), Some("content-filter"));
        }
        other => panic!("expected Finish, got: {other:?}"),
    }
}

// ============================================================================
// Error & Edge Cases (6)
// ============================================================================

// 35.
#[test]
fn provider_error_produces_error_event() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::Error {
        message: "provider returned 500".into(),
        code: Some("PROVIDER_ERROR".into()),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::Error { error_text } => {
            assert_eq!(error_text, "provider returned 500");
        }
        other => panic!("expected Error, got: {other:?}"),
    }
}

// 36.
#[test]
fn max_rounds_termination() {
    let mut enc = make_encoder_with_run("t1", "r1");
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

// 37.
#[test]
fn large_payload_not_truncated() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let mut obj = serde_json::Map::new();
    for i in 0..100 {
        obj.insert(format!("field_{i}"), json!(format!("value_{i}")));
    }
    let large_state = serde_json::Value::Object(obj);
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: large_state.clone(),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::Data {
            data_type, data, ..
        } => {
            assert_eq!(data_type, "data-state-snapshot");
            assert_eq!(*data, large_state);
            assert_eq!(data.as_object().unwrap().len(), 100);
            assert_eq!(data["field_0"], "value_0");
            assert_eq!(data["field_99"], "value_99");
        }
        other => panic!("expected Data state-snapshot, got: {other:?}"),
    }
}

// 38.
#[test]
fn special_characters_in_content() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let text = "Line1\nLine2\tTab\\Backslash\"Quote\0Null";
    let events = enc.on_agent_event(&AgentEvent::TextDelta { delta: text.into() });
    assert_eq!(events.len(), 2);
    match &events[1] {
        UIStreamEvent::TextDelta { delta, .. } => {
            assert_eq!(delta, text);
            assert!(delta.contains('\n'));
            assert!(delta.contains('\t'));
            assert!(delta.contains('\\'));
            assert!(delta.contains('"'));
        }
        other => panic!("expected TextDelta, got: {other:?}"),
    }
    // Verify serde roundtrip preserves special chars
    let json_str = serde_json::to_string(&events[1]).unwrap();
    let roundtripped: UIStreamEvent = serde_json::from_str(&json_str).unwrap();
    assert_eq!(roundtripped, events[1]);
}

// 39.
#[test]
fn reasoning_delta_forwarded() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ReasoningDelta {
        delta: "Let me think step by step...".into(),
    });
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], UIStreamEvent::ReasoningStart { .. }));
    match &events[1] {
        UIStreamEvent::ReasoningDelta { delta, .. } => {
            assert_eq!(delta, "Let me think step by step...");
        }
        other => panic!("expected ReasoningDelta, got: {other:?}"),
    }
}

// 40.
#[test]
fn reasoning_with_encrypted_value() {
    let mut enc = make_encoder_with_run("t1", "r1");
    let events = enc.on_agent_event(&AgentEvent::ReasoningEncryptedValue {
        encrypted_value: "encrypted-opaque-token-abc123".into(),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        UIStreamEvent::Data {
            data_type,
            data,
            id,
            transient,
        } => {
            assert_eq!(data_type, "data-reasoning-encrypted");
            assert_eq!(data["encryptedValue"], "encrypted-opaque-token-abc123");
            assert!(id.is_some(), "reasoning encrypted should have an id");
            assert_eq!(*transient, Some(true));
        }
        other => panic!("expected Data reasoning-encrypted, got: {other:?}"),
    }
}

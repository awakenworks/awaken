//! Full event lifecycle contract tests for the ACP encoder.

use serde_json::json;
use tirea_contract::{AgentEvent, StoppedReason, TerminationReason, Transcoder};
use tirea_protocol_acp::{AcpEncoder, AcpEvent, StopReason, ToolCallStatus};

// ==========================================================================
// Transcoder trait integration
// ==========================================================================

#[test]
fn transcoder_trait_delegates_to_on_agent_event() {
    let mut enc = AcpEncoder::new();
    let events = enc.transcode(&AgentEvent::TextDelta { delta: "hi".into() });
    assert_eq!(events, vec![AcpEvent::agent_message("hi")]);
}

// ==========================================================================
// Full text → tool → text → finish lifecycle
// ==========================================================================

#[test]
fn full_lifecycle_text_tool_text_finish() {
    let mut enc = AcpEncoder::new();

    // 1. RunStart — silently consumed
    let ev = enc.transcode(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    });
    assert!(ev.is_empty(), "RunStart should be silent");

    // 2. StepStart — silently consumed
    let ev = enc.transcode(&AgentEvent::StepStart {
        message_id: "msg_1".into(),
    });
    assert!(ev.is_empty(), "StepStart should be silent");

    // 3. Text streaming
    let ev = enc.transcode(&AgentEvent::TextDelta {
        delta: "Hello ".into(),
    });
    assert_eq!(ev, vec![AcpEvent::agent_message("Hello ")]);

    let ev = enc.transcode(&AgentEvent::TextDelta {
        delta: "world".into(),
    });
    assert_eq!(ev, vec![AcpEvent::agent_message("world")]);

    // 4. Tool call lifecycle
    let ev = enc.transcode(&AgentEvent::ToolCallStart {
        id: "call_1".into(),
        name: "search".into(),
    });
    assert!(ev.is_empty(), "ToolCallStart should be buffered");

    let ev = enc.transcode(&AgentEvent::ToolCallDelta {
        id: "call_1".into(),
        args_delta: "{\"q\":".into(),
    });
    assert!(ev.is_empty(), "ToolCallDelta should be buffered");

    let ev = enc.transcode(&AgentEvent::ToolCallReady {
        id: "call_1".into(),
        name: "search".into(),
        arguments: json!({"q": "rust"}),
    });
    assert_eq!(ev.len(), 1);
    assert_eq!(
        ev[0],
        AcpEvent::tool_call("call_1", "search", json!({"q": "rust"}))
    );

    let ev = enc.transcode(&AgentEvent::ToolCallDone {
        id: "call_1".into(),
        result: tirea_contract::runtime::tool_call::ToolResult::success(
            "search",
            json!({"results": [1, 2, 3]}),
        ),
        patch: None,
        message_id: "msg_tool_1".into(),
    });
    assert_eq!(ev.len(), 1);
    match &ev[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.id, "call_1");
            assert_eq!(update.status, ToolCallStatus::Completed);
        }
        other => panic!("expected tool_call_update, got: {other:?}"),
    }

    // 5. More text
    let ev = enc.transcode(&AgentEvent::TextDelta {
        delta: "Found 3 results.".into(),
    });
    assert_eq!(ev, vec![AcpEvent::agent_message("Found 3 results.")]);

    // 6. Finish
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::EndTurn)]);
}

// ==========================================================================
// Terminal guard: events after finish are suppressed
// ==========================================================================

#[test]
fn events_after_run_finish_are_suppressed() {
    let mut enc = AcpEncoder::new();

    let _ = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });

    assert!(enc
        .transcode(&AgentEvent::TextDelta {
            delta: "late".into()
        })
        .is_empty());
    assert!(enc
        .transcode(&AgentEvent::ToolCallReady {
            id: "c".into(),
            name: "x".into(),
            arguments: json!({}),
        })
        .is_empty());
    assert!(enc
        .transcode(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        })
        .is_empty());
}

#[test]
fn events_after_error_are_suppressed() {
    let mut enc = AcpEncoder::new();

    let _ = enc.transcode(&AgentEvent::Error {
        message: "fatal".into(),
        code: None,
    });

    assert!(enc
        .transcode(&AgentEvent::TextDelta {
            delta: "late".into()
        })
        .is_empty());
}

// ==========================================================================
// Termination reason mapping
// ==========================================================================

#[test]
fn behavior_requested_maps_to_end_turn() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::BehaviorRequested,
    });
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::EndTurn)]);
}

#[test]
fn timeout_reached_maps_to_max_tokens() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Stopped(StoppedReason::new("timeout_reached")),
    });
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::MaxTokens)]);
}

#[test]
fn token_budget_exceeded_maps_to_max_tokens() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Stopped(StoppedReason::new("token_budget_exceeded")),
    });
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::MaxTokens)]);
}

#[test]
fn unknown_stopped_code_maps_to_end_turn() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Stopped(StoppedReason::new("tool_called")),
    });
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::EndTurn)]);
}

// ==========================================================================
// Silently consumed events
// ==========================================================================

#[test]
fn inference_complete_silently_consumed() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::InferenceComplete {
        model: "claude".into(),
        usage: None,
        duration_ms: 100,
    });
    assert!(ev.is_empty());
}

#[test]
fn step_events_silently_consumed() {
    let mut enc = AcpEncoder::new();
    assert!(enc
        .transcode(&AgentEvent::StepStart {
            message_id: "m".into()
        })
        .is_empty());
    assert!(enc.transcode(&AgentEvent::StepEnd).is_empty());
}

#[test]
fn activity_snapshot_forwarded() {
    let mut enc = AcpEncoder::new();
    let events = enc.transcode(&AgentEvent::ActivitySnapshot {
        message_id: "m".into(),
        activity_type: "thinking".into(),
        content: json!({"text": "processing"}),
        replace: Some(true),
    });
    assert_eq!(events.len(), 1);
    let value = serde_json::to_value(&events[0]).unwrap();
    assert_eq!(value["params"]["activity"]["messageId"], "m");
    assert_eq!(value["params"]["activity"]["activityType"], "thinking");
    assert_eq!(value["params"]["activity"]["content"]["text"], "processing");
    assert_eq!(value["params"]["activity"]["replace"], true);
}

#[test]
fn activity_delta_forwarded() {
    let mut enc = AcpEncoder::new();
    let patch = vec![json!({"op": "replace", "path": "/progress", "value": 50})];
    let events = enc.transcode(&AgentEvent::ActivityDelta {
        message_id: "m".into(),
        activity_type: "tool_call_progress".into(),
        patch: patch.clone(),
    });
    assert_eq!(events.len(), 1);
    let value = serde_json::to_value(&events[0]).unwrap();
    assert_eq!(value["params"]["activity"]["messageId"], "m");
    assert_eq!(
        value["params"]["activity"]["activityType"],
        "tool_call_progress"
    );
    assert_eq!(value["params"]["activity"]["patch"], json!(patch));
}

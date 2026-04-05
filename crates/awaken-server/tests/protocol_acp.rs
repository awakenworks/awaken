//! ACP encoder contract tests — migrated from tirea-protocol-acp.
//!
//! Validates event mapping, termination reason mapping, permission flow,
//! state snapshot/delta visibility, tool call lifecycle, and terminal guard.
//!
//! Updated for ACP spec compliance:
//! - ToolCallStatus: pending/in_progress/completed/failed (no denied/errored)
//! - StopReason: end_turn/max_tokens/max_turn_requests/refusal/cancelled (no error/suspended)
//! - PermissionOption: struct with optionId/name/kind
//! - Tool calls use tool_call_update with toolCallId/title/kind/rawInput/rawOutput

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::lifecycle::{StoppedReason, TerminationReason};
use awaken_contract::contract::suspension::ToolCallOutcome;
use awaken_contract::contract::tool::ToolResult;
use awaken_contract::contract::transport::Transcoder;
use awaken_server::protocols::acp::encoder::{AcpEncoder, AcpEvent};
use awaken_server::protocols::acp::types::{
    PermissionOptionKind, StopReason, ToolCallKind, ToolCallStatus,
};
use serde_json::json;

// ============================================================================
// Transcoder trait integration
// ============================================================================

#[test]
fn transcoder_trait_delegates_to_on_agent_event() {
    let mut enc = AcpEncoder::new();
    let events = enc.transcode(&AgentEvent::TextDelta { delta: "hi".into() });
    assert_eq!(events, vec![AcpEvent::agent_message("hi")]);
}

// ============================================================================
// Full lifecycle: text → tool → text → finish
// ============================================================================

#[test]
fn full_lifecycle_text_tool_text_finish() {
    let mut enc = AcpEncoder::new();

    // RunStart — silently consumed
    let ev = enc.transcode(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    });
    assert!(ev.is_empty());

    // StepStart — silently consumed
    let ev = enc.transcode(&AgentEvent::StepStart {
        message_id: "msg_1".into(),
    });
    assert!(ev.is_empty());

    // Text streaming
    let ev = enc.transcode(&AgentEvent::TextDelta {
        delta: "Hello ".into(),
    });
    assert_eq!(ev, vec![AcpEvent::agent_message("Hello ")]);

    let ev = enc.transcode(&AgentEvent::TextDelta {
        delta: "world".into(),
    });
    assert_eq!(ev, vec![AcpEvent::agent_message("world")]);

    // Tool call lifecycle
    let ev = enc.transcode(&AgentEvent::ToolCallStart {
        id: "call_1".into(),
        name: "search".into(),
    });
    assert!(ev.is_empty(), "ToolCallStart should be buffered");

    let ev = enc.transcode(&AgentEvent::ToolCallDelta {
        id: "call_1".into(),
        args_delta: r#"{"q":"#.into(),
    });
    assert!(ev.is_empty(), "ToolCallDelta should be buffered");

    let ev = enc.transcode(&AgentEvent::ToolCallReady {
        id: "call_1".into(),
        name: "search".into(),
        arguments: json!({"q": "rust"}),
    });
    assert_eq!(ev.len(), 1);
    match &ev[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.tool_call_id, "call_1");
            assert_eq!(update.status, ToolCallStatus::Pending);
            assert_eq!(update.title.as_deref(), Some("search"));
            assert_eq!(update.kind, Some(ToolCallKind::Search));
            assert_eq!(update.raw_input, Some(json!({"q": "rust"})));
        }
        other => panic!("expected pending tool_call_update, got: {other:?}"),
    }

    let ev = enc.transcode(&AgentEvent::ToolCallDone {
        id: "call_1".into(),
        message_id: "msg_tool_1".into(),
        result: ToolResult::success("search", json!({"results": [1, 2, 3]})),
        outcome: ToolCallOutcome::Succeeded,
    });
    assert_eq!(ev.len(), 1);
    match &ev[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.tool_call_id, "call_1");
            assert_eq!(update.status, ToolCallStatus::Completed);
        }
        other => panic!("expected tool_call_update, got: {other:?}"),
    }

    // More text
    let ev = enc.transcode(&AgentEvent::TextDelta {
        delta: "Found 3 results.".into(),
    });
    assert_eq!(ev, vec![AcpEvent::agent_message("Found 3 results.")]);

    // Finish
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::EndTurn)]);
}

// ============================================================================
// Terminal guard
// ============================================================================

#[test]
fn events_after_run_finish_are_suppressed() {
    let mut enc = AcpEncoder::new();
    enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });

    assert!(
        enc.transcode(&AgentEvent::TextDelta {
            delta: "late".into()
        })
        .is_empty()
    );
    assert!(
        enc.transcode(&AgentEvent::ToolCallReady {
            id: "c".into(),
            name: "x".into(),
            arguments: json!({}),
        })
        .is_empty()
    );
    assert!(
        enc.transcode(&AgentEvent::RunFinish {
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
    let mut enc = AcpEncoder::new();
    enc.transcode(&AgentEvent::Error {
        message: "fatal".into(),
        code: None,
    });
    assert!(
        enc.transcode(&AgentEvent::TextDelta {
            delta: "late".into()
        })
        .is_empty()
    );
}

// ============================================================================
// Termination reason mapping
// ============================================================================

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
fn cancelled_maps_to_cancelled() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Cancelled,
    });
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::Cancelled)]);
}

#[test]
fn suspended_maps_to_cancelled() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Suspended,
    });
    // Suspended has no spec equivalent; mapped to cancelled
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::Cancelled)]);
}

#[test]
fn error_termination_emits_error_then_finished() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Error("boom".into()),
    });
    assert_eq!(ev.len(), 2);
    assert_eq!(ev[0], AcpEvent::error("boom", None));
    // Error maps to end_turn (error details in the preceding error event)
    assert_eq!(ev[1], AcpEvent::finished(StopReason::EndTurn));
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
fn max_rounds_reached_maps_to_max_tokens() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Stopped(StoppedReason::new("max_rounds_reached")),
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

#[test]
fn blocked_maps_to_refusal() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Blocked("unsafe tool".into()),
    });
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::Refusal)]);
}

// ============================================================================
// Silently consumed events
// ============================================================================

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
    assert!(
        enc.transcode(&AgentEvent::StepStart {
            message_id: "m".into()
        })
        .is_empty()
    );
    assert!(enc.transcode(&AgentEvent::StepEnd).is_empty());
}

#[test]
fn run_start_silently_consumed() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    });
    assert!(ev.is_empty());
}

#[test]
fn reasoning_encrypted_value_silently_consumed() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::ReasoningEncryptedValue {
        encrypted_value: "opaque".into(),
    });
    assert!(ev.is_empty());
}

#[test]
fn messages_snapshot_silently_consumed() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::MessagesSnapshot {
        messages: vec![json!({"role": "user"})],
    });
    assert!(ev.is_empty());
}

// ============================================================================
// Reasoning delta maps to agent_thought
// ============================================================================

#[test]
fn reasoning_delta_maps_to_agent_thought() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::ReasoningDelta {
        delta: "thinking".into(),
    });
    assert_eq!(ev, vec![AcpEvent::agent_thought("thinking")]);
}

// ============================================================================
// Tool call error
// ============================================================================

#[test]
fn tool_call_done_error_maps_to_failed() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::error("search", "backend failure"),
        outcome: ToolCallOutcome::Failed,
    });
    assert_eq!(ev.len(), 1);
    match &ev[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.status, ToolCallStatus::Failed);
            assert_eq!(update.error.as_deref(), Some("backend failure"));
        }
        other => panic!("expected failed update, got: {other:?}"),
    }
}

// ============================================================================
// Permission flow (PermissionConfirm tool)
// ============================================================================

#[test]
fn permission_confirm_tool_emits_request_permission() {
    let mut enc = AcpEncoder::new().with_session_id("sess_test");
    let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "fc_perm_1".into(),
        name: "PermissionConfirm".into(),
        arguments: json!({
            "tool_name": "bash",
            "tool_args": {"command": "rm -rf /tmp/test"}
        }),
    });

    assert_eq!(
        events.len(),
        2,
        "should emit tool_call_update (pending) + request_permission"
    );

    // First event: pending tool call update
    match &events[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.tool_call_id, "fc_perm_1");
            assert_eq!(update.status, ToolCallStatus::Pending);
        }
        other => panic!("expected pending tool_call_update, got: {other:?}"),
    }

    // Second event: request_permission RPC
    match &events[1] {
        AcpEvent::RequestPermission(params) => {
            assert_eq!(params.session_id, "sess_test");
            assert_eq!(params.tool_call.tool_call_id, "fc_perm_1");
            assert_eq!(params.options.len(), 4);
            assert_eq!(params.options[0].option_id, "opt_allow_once");
            assert_eq!(params.options[0].kind, PermissionOptionKind::AllowOnce);
            assert_eq!(params.options[1].option_id, "opt_allow_always");
            assert_eq!(params.options[1].kind, PermissionOptionKind::AllowAlways);
            assert_eq!(params.options[2].option_id, "opt_reject_once");
            assert_eq!(params.options[2].kind, PermissionOptionKind::RejectOnce);
            assert_eq!(params.options[3].option_id, "opt_reject_always");
            assert_eq!(params.options[3].kind, PermissionOptionKind::RejectAlways);
        }
        other => panic!("expected RequestPermission, got: {other:?}"),
    }
}

#[test]
fn permission_confirm_case_insensitive() {
    let mut enc = AcpEncoder::new().with_session_id("sess_test");
    let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "fc_2".into(),
        name: "permissionconfirm".into(),
        arguments: json!({"tool_name": "echo", "tool_args": {}}),
    });
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[1], AcpEvent::RequestPermission(_)));
}

#[test]
fn non_permission_tool_does_not_emit_request_permission() {
    let mut enc = AcpEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "call_1".into(),
        name: "search".into(),
        arguments: json!({"q": "rust"}),
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], AcpEvent::SessionUpdate(_)));
}

#[test]
fn permission_confirm_missing_tool_args_uses_null() {
    let mut enc = AcpEncoder::new().with_session_id("sess_test");
    let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "fc_3".into(),
        name: "PermissionConfirm".into(),
        arguments: json!({"tool_name": "echo"}),
    });
    assert_eq!(events.len(), 2);
    match &events[1] {
        AcpEvent::RequestPermission(params) => {
            assert!(
                params.tool_call.raw_input.as_ref().unwrap().is_null(),
                "missing tool_args should be null"
            );
        }
        other => panic!("expected RequestPermission, got: {other:?}"),
    }
}

#[test]
fn permission_confirm_missing_tool_name_uses_unknown() {
    let mut enc = AcpEncoder::new().with_session_id("sess_test");
    let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "fc_4".into(),
        name: "PermissionConfirm".into(),
        arguments: json!({"tool_args": {"x": 1}}),
    });
    assert_eq!(events.len(), 2);
    match &events[1] {
        AcpEvent::RequestPermission(params) => {
            assert_eq!(params.tool_call.title.as_deref(), Some("unknown"));
        }
        other => panic!("expected RequestPermission, got: {other:?}"),
    }
}

// ============================================================================
// Permission resolution
// ============================================================================

#[test]
fn approved_resolution_maps_to_completed() {
    let mut enc = AcpEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
        target_id: "fc_perm_1".into(),
        result: json!({"approved": true}),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.tool_call_id, "fc_perm_1");
            assert_eq!(update.status, ToolCallStatus::Completed);
            assert!(update.raw_output.is_some());
        }
        other => panic!("expected completed update, got: {other:?}"),
    }
}

#[test]
fn denied_resolution_maps_to_failed_status() {
    let mut enc = AcpEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
        target_id: "fc_perm_1".into(),
        result: json!({"approved": false, "reason": "user rejected"}),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.tool_call_id, "fc_perm_1");
            // Spec has no "denied" status — mapped to failed
            assert_eq!(update.status, ToolCallStatus::Failed);
            assert_eq!(update.error.as_deref(), Some("permission denied"));
        }
        other => panic!("expected failed update, got: {other:?}"),
    }
}

#[test]
fn error_resolution_maps_to_failed_status() {
    let mut enc = AcpEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
        target_id: "fc_perm_1".into(),
        result: json!({"error": "frontend validation failed"}),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.tool_call_id, "fc_perm_1");
            assert_eq!(update.status, ToolCallStatus::Failed);
            assert_eq!(update.error.as_deref(), Some("frontend validation failed"));
        }
        other => panic!("expected failed update, got: {other:?}"),
    }
}

// ============================================================================
// State snapshot visibility
// ============================================================================

#[test]
fn state_snapshot_with_permission_overrides_is_forwarded() {
    let mut enc = AcpEncoder::new();
    let snapshot = json!({
        "permission_overrides": {
            "allow_tools": ["bash", "search"],
            "deny_tools": []
        },
        "other_state": {"key": "value"}
    });

    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: snapshot.clone(),
    });

    assert_eq!(events.len(), 1);
    match &events[0] {
        AcpEvent::SessionUpdate(params) => {
            let snap = params
                .state_snapshot
                .as_ref()
                .expect("expected state_snapshot");
            assert_eq!(snap, &snapshot);
            assert!(snap.get("permission_overrides").is_some());
            assert_eq!(snap["permission_overrides"]["allow_tools"][0], "bash");
            assert_eq!(snap["permission_overrides"]["allow_tools"][1], "search");
        }
        other => panic!("expected state_snapshot, got: {other:?}"),
    }
}

#[test]
fn state_delta_reflecting_permission_override_addition() {
    let mut enc = AcpEncoder::new();
    let delta = vec![json!({
        "op": "add",
        "path": "/permission_overrides/allow_tools/-",
        "value": "bash"
    })];

    let events = enc.on_agent_event(&AgentEvent::StateDelta {
        delta: delta.clone(),
    });

    assert_eq!(events.len(), 1);
    match &events[0] {
        AcpEvent::SessionUpdate(params) => {
            let d = params.state_delta.as_ref().expect("expected state_delta");
            assert_eq!(d, &delta);
            assert_eq!(d[0]["path"], "/permission_overrides/allow_tools/-");
        }
        other => panic!("expected state_delta, got: {other:?}"),
    }
}

#[test]
fn state_delta_reflecting_permission_override_removal() {
    let mut enc = AcpEncoder::new();
    let delta = vec![json!({
        "op": "remove",
        "path": "/permission_overrides/allow_tools/0"
    })];

    let events = enc.on_agent_event(&AgentEvent::StateDelta {
        delta: delta.clone(),
    });

    assert_eq!(events.len(), 1);
    match &events[0] {
        AcpEvent::SessionUpdate(params) => {
            let d = params.state_delta.as_ref().expect("expected state_delta");
            assert_eq!(d[0]["op"], "remove");
        }
        other => panic!("expected state_delta, got: {other:?}"),
    }
}

#[test]
fn state_snapshot_after_cleanup_has_no_overrides() {
    let mut enc = AcpEncoder::new();

    // First snapshot with overrides
    enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({
            "permission_overrides": {"allow_tools": ["bash"]},
            "other": "data"
        }),
    });

    // Second snapshot without overrides
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({"other": "data"}),
    });

    assert_eq!(events.len(), 1);
    match &events[0] {
        AcpEvent::SessionUpdate(params) => {
            let snap = params
                .state_snapshot
                .as_ref()
                .expect("expected state_snapshot");
            assert!(snap.get("permission_overrides").is_none());
        }
        other => panic!("expected state_snapshot, got: {other:?}"),
    }
}

#[test]
fn multiple_sequential_state_deltas() {
    let mut enc = AcpEncoder::new();

    let delta1 = vec![json!({
        "op": "replace",
        "path": "/permission_overrides",
        "value": {"allow_tools": ["bash"]}
    })];
    let delta2 = vec![json!({
        "op": "replace",
        "path": "/permission_overrides",
        "value": {"allow_tools": ["bash", "search"]}
    })];

    let ev1 = enc.on_agent_event(&AgentEvent::StateDelta { delta: delta1 });
    let ev2 = enc.on_agent_event(&AgentEvent::StateDelta { delta: delta2 });

    assert_eq!(ev1.len(), 1);
    assert_eq!(ev2.len(), 1);

    match (&ev1[0], &ev2[0]) {
        (AcpEvent::SessionUpdate(p1), AcpEvent::SessionUpdate(p2)) => {
            let d1 = p1.state_delta.as_ref().expect("expected state_delta");
            let d2 = p2.state_delta.as_ref().expect("expected state_delta");
            assert_eq!(d1[0]["value"]["allow_tools"][0], "bash");
            assert_eq!(d2[0]["value"]["allow_tools"][1], "search");
        }
        other => panic!("expected two state_deltas, got: {other:?}"),
    }
}

#[test]
fn empty_state_snapshot_forwarded() {
    let mut enc = AcpEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({}),
    });
    assert_eq!(events.len(), 1);
    assert_eq!(events[0], AcpEvent::state_snapshot(json!({})));
}

// ============================================================================
// Activity events
// ============================================================================

#[test]
fn activity_snapshot_forwarded() {
    let mut enc = AcpEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
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
    let events = enc.on_agent_event(&AgentEvent::ActivityDelta {
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

// ============================================================================
// Serde roundtrips
// ============================================================================

#[test]
fn session_update_roundtrip() {
    let event = AcpEvent::agent_message("hello");
    let json = serde_json::to_string(&event).unwrap();
    let restored: AcpEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(event, restored);
}

#[test]
fn request_permission_roundtrip() {
    let event = AcpEvent::request_permission("sess_1", "fc_1", "bash", json!({"command": "rm"}));
    let json = serde_json::to_string(&event).unwrap();
    let restored: AcpEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(event, restored);
}

#[test]
fn finished_serializes_stop_reason() {
    let event = AcpEvent::finished(StopReason::EndTurn);
    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(value["params"]["finished"]["stopReason"], "end_turn");
}

#[test]
fn tool_call_status_serde_roundtrip() {
    for status in [
        ToolCallStatus::Pending,
        ToolCallStatus::InProgress,
        ToolCallStatus::Completed,
        ToolCallStatus::Failed,
    ] {
        let json = serde_json::to_string(&status).unwrap();
        let parsed: ToolCallStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, status);
    }
}

#[test]
fn stop_reason_serde_roundtrip() {
    for reason in [
        StopReason::EndTurn,
        StopReason::MaxTokens,
        StopReason::MaxTurnRequests,
        StopReason::Refusal,
        StopReason::Cancelled,
    ] {
        let json = serde_json::to_string(&reason).unwrap();
        let parsed: StopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, reason);
    }
}

#[test]
fn permission_option_serde_roundtrip() {
    use awaken_server::protocols::acp::types::PermissionOption;
    let opt = PermissionOption {
        option_id: "opt_allow_once".into(),
        name: "Allow once".into(),
        kind: PermissionOptionKind::AllowOnce,
    };
    let json = serde_json::to_string(&opt).unwrap();
    let parsed: PermissionOption = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, opt);
}

// ============================================================================
// Error event
// ============================================================================

#[test]
fn error_event_sets_terminal_guard() {
    let mut enc = AcpEncoder::new();
    let ev = enc.on_agent_event(&AgentEvent::Error {
        message: "fatal".into(),
        code: Some("E001".into()),
    });
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0], AcpEvent::error("fatal", Some("E001".into())));

    // Terminal guard active
    assert!(
        enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "ignored".into()
        })
        .is_empty()
    );
}

// ============================================================================
// Tool Execution
// ============================================================================

#[test]
fn tool_call_result_has_correct_payload() {
    let mut enc = AcpEncoder::new();
    let result_data = json!({"files": ["a.rs", "b.rs"], "count": 2});
    let ev = enc.transcode(&AgentEvent::ToolCallDone {
        id: "call_42".into(),
        message_id: "msg_42".into(),
        result: ToolResult::success("file_search", result_data.clone()),
        outcome: ToolCallOutcome::Succeeded,
    });
    assert_eq!(ev.len(), 1);
    match &ev[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.tool_call_id, "call_42");
            assert_eq!(update.status, ToolCallStatus::Completed);
            let result_val = update.raw_output.as_ref().unwrap();
            assert!(result_val.get("data").is_some() || result_val.get("files").is_some());
        }
        other => panic!("expected tool_call_update, got: {other:?}"),
    }
}

#[test]
fn multiple_tool_calls_produce_results() {
    let mut enc = AcpEncoder::new();

    // First tool call
    let ev1 = enc.transcode(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "read_file".into(),
        arguments: json!({"path": "/tmp/a.txt"}),
    });
    assert_eq!(ev1.len(), 1);

    let ev2 = enc.transcode(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::success("read_file", json!("contents of a")),
        outcome: ToolCallOutcome::Succeeded,
    });
    assert_eq!(ev2.len(), 1);

    // Second tool call
    let ev3 = enc.transcode(&AgentEvent::ToolCallReady {
        id: "c2".into(),
        name: "read_file".into(),
        arguments: json!({"path": "/tmp/b.txt"}),
    });
    assert_eq!(ev3.len(), 1);

    let ev4 = enc.transcode(&AgentEvent::ToolCallDone {
        id: "c2".into(),
        message_id: "m2".into(),
        result: ToolResult::success("read_file", json!("contents of b")),
        outcome: ToolCallOutcome::Succeeded,
    });
    assert_eq!(ev4.len(), 1);

    // Verify both results reference correct call IDs
    match (&ev2[0], &ev4[0]) {
        (AcpEvent::SessionUpdate(p1), AcpEvent::SessionUpdate(p2)) => {
            assert_eq!(p1.tool_call_update.as_ref().unwrap().tool_call_id, "c1");
            assert_eq!(p2.tool_call_update.as_ref().unwrap().tool_call_id, "c2");
        }
        other => panic!("expected two SessionUpdates, got: {other:?}"),
    }
}

#[test]
fn failed_tool_produces_error_content() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::ToolCallDone {
        id: "c_fail".into(),
        message_id: "m_fail".into(),
        result: ToolResult::error("database_query", "connection refused"),
        outcome: ToolCallOutcome::Failed,
    });
    assert_eq!(ev.len(), 1);
    match &ev[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.tool_call_id, "c_fail");
            assert_eq!(update.status, ToolCallStatus::Failed);
            assert_eq!(update.error.as_deref(), Some("connection refused"));
            assert!(update.raw_output.is_none());
        }
        other => panic!("expected failed update, got: {other:?}"),
    }
}

#[test]
fn tool_with_complex_arguments() {
    let mut enc = AcpEncoder::new();
    let complex_args = json!({
        "query": "SELECT * FROM users WHERE age > 18",
        "params": [18, "active"],
        "options": {
            "timeout_ms": 5000,
            "retry": true,
            "nested": {"deep": {"value": [1, 2, 3]}}
        }
    });
    let ev = enc.transcode(&AgentEvent::ToolCallReady {
        id: "c_complex".into(),
        name: "sql_query".into(),
        arguments: complex_args.clone(),
    });
    assert_eq!(ev.len(), 1);
    match &ev[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.title.as_deref(), Some("sql_query"));
            let raw_input = update.raw_input.as_ref().unwrap();
            assert_eq!(raw_input, &complex_args);
            assert_eq!(raw_input["options"]["nested"]["deep"]["value"][2], 3);
        }
        other => panic!("expected tool_call_update, got: {other:?}"),
    }
}

#[test]
fn tool_suspension_handling() {
    let mut enc = AcpEncoder::new();

    // Tool call ready
    let ev = enc.transcode(&AgentEvent::ToolCallReady {
        id: "c_suspend".into(),
        name: "approval_gate".into(),
        arguments: json!({"action": "deploy"}),
    });
    assert_eq!(ev.len(), 1);

    // Run finishes with Suspended termination → maps to Cancelled
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Suspended,
    });
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::Cancelled)]);
}

#[test]
fn tool_result_contains_call_id() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::ToolCallDone {
        id: "unique_call_id_abc123".into(),
        message_id: "msg_x".into(),
        result: ToolResult::success("echo", json!("ok")),
        outcome: ToolCallOutcome::Succeeded,
    });
    match &ev[0] {
        AcpEvent::SessionUpdate(params) => {
            assert_eq!(
                params.tool_call_update.as_ref().unwrap().tool_call_id,
                "unique_call_id_abc123"
            );
        }
        other => panic!("expected SessionUpdate, got: {other:?}"),
    }
}

#[test]
fn tool_call_incremental_events_skipped() {
    let mut enc = AcpEncoder::new();

    // ToolCallStart -> skipped
    assert!(
        enc.transcode(&AgentEvent::ToolCallStart {
            id: "c1".into(),
            name: "search".into(),
        })
        .is_empty()
    );

    // ToolCallDelta -> skipped
    assert!(
        enc.transcode(&AgentEvent::ToolCallDelta {
            id: "c1".into(),
            args_delta: r#"{"q":"#.into(),
        })
        .is_empty()
    );

    // Another delta -> skipped
    assert!(
        enc.transcode(&AgentEvent::ToolCallDelta {
            id: "c1".into(),
            args_delta: r#""rust"}"#.into(),
        })
        .is_empty()
    );

    // Only ToolCallReady produces output
    let ev = enc.transcode(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "search".into(),
        arguments: json!({"q": "rust"}),
    });
    assert_eq!(ev.len(), 1);
}

#[test]
fn tool_result_is_final_only() {
    let mut enc = AcpEncoder::new();

    // ToolCallStart produces nothing
    let ev_start = enc.transcode(&AgentEvent::ToolCallStart {
        id: "c1".into(),
        name: "search".into(),
    });
    assert!(ev_start.is_empty());

    // ToolCallDelta produces nothing
    let ev_delta = enc.transcode(&AgentEvent::ToolCallDelta {
        id: "c1".into(),
        args_delta: r#"{"q": "test"}"#.into(),
    });
    assert!(ev_delta.is_empty());

    // ToolCallReady produces the pending tool_call_update event
    let ev_ready = enc.transcode(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "search".into(),
        arguments: json!({"q": "test"}),
    });
    assert_eq!(ev_ready.len(), 1);

    // ToolCallDone produces the completed tool_call_update
    let ev_done = enc.transcode(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::success("search", json!([])),
        outcome: ToolCallOutcome::Succeeded,
    });
    assert_eq!(ev_done.len(), 1);
    match &ev_done[0] {
        AcpEvent::SessionUpdate(params) => {
            assert!(params.tool_call_update.is_some());
            assert_eq!(
                params.tool_call_update.as_ref().unwrap().status,
                ToolCallStatus::Completed
            );
        }
        other => panic!("expected SessionUpdate with tool_call_update, got: {other:?}"),
    }
}

// ============================================================================
// Text & Message
// ============================================================================

#[test]
fn text_content_forwarded() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::TextDelta {
        delta: "Here is some text content.".into(),
    });
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0], AcpEvent::agent_message("Here is some text content."));
}

#[test]
fn multiple_text_blocks_accumulated() {
    let mut enc = AcpEncoder::new();
    let ev1 = enc.transcode(&AgentEvent::TextDelta {
        delta: "First ".into(),
    });
    let ev2 = enc.transcode(&AgentEvent::TextDelta {
        delta: "second ".into(),
    });
    let ev3 = enc.transcode(&AgentEvent::TextDelta {
        delta: "third.".into(),
    });

    assert_eq!(ev1, vec![AcpEvent::agent_message("First ")]);
    assert_eq!(ev2, vec![AcpEvent::agent_message("second ")]);
    assert_eq!(ev3, vec![AcpEvent::agent_message("third.")]);
}

#[test]
fn run_finish_with_result() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: Some(json!({"response": {"text": "Final answer"}})),
        termination: TerminationReason::NaturalEnd,
    });
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::EndTurn)]);
}

#[test]
fn run_finish_cancelled() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Cancelled,
    });
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::Cancelled)]);
}

#[test]
fn run_error_forwarded() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::Error {
        message: "provider unreachable".into(),
        code: Some("PROVIDER_DOWN".into()),
    });
    assert_eq!(ev.len(), 1);
    assert_eq!(
        ev[0],
        AcpEvent::error("provider unreachable", Some("PROVIDER_DOWN".into()))
    );
}

#[test]
fn empty_response_handling() {
    let mut enc = AcpEncoder::new();

    // Empty text delta still produces an event
    let ev = enc.transcode(&AgentEvent::TextDelta { delta: "".into() });
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0], AcpEvent::agent_message(""));

    // Run finish with empty result
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: Some(json!({})),
        termination: TerminationReason::NaturalEnd,
    });
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::EndTurn)]);
}

// ============================================================================
// State & Activity
// ============================================================================

#[test]
fn state_snapshot_forwarded_new() {
    let mut enc = AcpEncoder::new();
    let snapshot = json!({"model": "claude-4", "temperature": 0.7});
    let ev = enc.transcode(&AgentEvent::StateSnapshot {
        snapshot: snapshot.clone(),
    });
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0], AcpEvent::state_snapshot(snapshot));
}

#[test]
fn state_snapshot_nested_json() {
    let mut enc = AcpEncoder::new();
    let snapshot = json!({
        "level1": {
            "level2": {
                "level3": {
                    "level4": [1, 2, {"level5": true}]
                }
            }
        },
        "array_of_objects": [
            {"id": 1, "nested": {"a": "b"}},
            {"id": 2, "nested": {"c": "d"}}
        ]
    });
    let ev = enc.transcode(&AgentEvent::StateSnapshot {
        snapshot: snapshot.clone(),
    });
    assert_eq!(ev.len(), 1);
    match &ev[0] {
        AcpEvent::SessionUpdate(params) => {
            let snap = params.state_snapshot.as_ref().unwrap();
            assert_eq!(
                snap["level1"]["level2"]["level3"]["level4"][2]["level5"],
                true
            );
            assert_eq!(snap["array_of_objects"][1]["nested"]["c"], "d");
        }
        other => panic!("expected state_snapshot, got: {other:?}"),
    }
}

#[test]
fn activity_snapshot_forwarded_new() {
    let mut enc = AcpEncoder::new();
    let content = json!({"status": "running", "progress": 0.75});
    let ev = enc.transcode(&AgentEvent::ActivitySnapshot {
        message_id: "act_1".into(),
        activity_type: "progress".into(),
        content: content.clone(),
        replace: Some(false),
    });
    assert_eq!(ev.len(), 1);
    let value = serde_json::to_value(&ev[0]).unwrap();
    assert_eq!(value["params"]["activity"]["messageId"], "act_1");
    assert_eq!(value["params"]["activity"]["activityType"], "progress");
    assert_eq!(value["params"]["activity"]["content"]["progress"], 0.75);
    assert_eq!(value["params"]["activity"]["replace"], false);
}

#[test]
fn activity_delta_forwarded_new() {
    let mut enc = AcpEncoder::new();
    let patch = vec![
        json!({"op": "replace", "path": "/status", "value": "complete"}),
        json!({"op": "add", "path": "/duration_ms", "value": 1234}),
    ];
    let ev = enc.transcode(&AgentEvent::ActivityDelta {
        message_id: "act_2".into(),
        activity_type: "build_progress".into(),
        patch: patch.clone(),
    });
    assert_eq!(ev.len(), 1);
    let value = serde_json::to_value(&ev[0]).unwrap();
    assert_eq!(value["params"]["activity"]["messageId"], "act_2");
    assert_eq!(value["params"]["activity"]["patch"][0]["op"], "replace");
    assert_eq!(value["params"]["activity"]["patch"][1]["value"], 1234);
}

#[test]
fn messages_snapshot_forwarded_silently() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::MessagesSnapshot {
        messages: vec![
            json!({"role": "user", "content": "Hello"}),
            json!({"role": "assistant", "content": "Hi there"}),
        ],
    });
    assert!(ev.is_empty());
}

#[test]
fn state_snapshot_empty_new() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::StateSnapshot {
        snapshot: json!({}),
    });
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0], AcpEvent::state_snapshot(json!({})));
}

// ============================================================================
// Event Sequence
// ============================================================================

#[test]
fn events_start_with_lifecycle() {
    let mut enc = AcpEncoder::new();

    let ev = enc.transcode(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    });
    assert!(ev.is_empty(), "ACP silently consumes RunStart");

    let ev = enc.transcode(&AgentEvent::TextDelta {
        delta: "Hello".into(),
    });
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0], AcpEvent::agent_message("Hello"));
}

#[test]
fn events_end_with_finish() {
    let mut enc = AcpEncoder::new();

    enc.transcode(&AgentEvent::TextDelta {
        delta: "output".into(),
    });

    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });

    assert_eq!(ev.len(), 1);
    match &ev[0] {
        AcpEvent::SessionUpdate(params) => {
            assert!(params.finished.is_some());
            assert_eq!(
                params.finished.as_ref().unwrap().stop_reason,
                StopReason::EndTurn
            );
        }
        other => panic!("expected finished event, got: {other:?}"),
    }
}

#[test]
fn terminal_guard_suppresses_after_finish() {
    let mut enc = AcpEncoder::new();

    enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });

    assert!(
        enc.transcode(&AgentEvent::TextDelta { delta: "a".into() })
            .is_empty()
    );
    assert!(
        enc.transcode(&AgentEvent::ToolCallReady {
            id: "c".into(),
            name: "x".into(),
            arguments: json!({}),
        })
        .is_empty()
    );
    assert!(
        enc.transcode(&AgentEvent::StateSnapshot {
            snapshot: json!({"key": "val"}),
        })
        .is_empty()
    );
    assert!(
        enc.transcode(&AgentEvent::ReasoningDelta {
            delta: "late thought".into(),
        })
        .is_empty()
    );
}

#[test]
fn terminal_guard_suppresses_after_error() {
    let mut enc = AcpEncoder::new();

    enc.transcode(&AgentEvent::Error {
        message: "crash".into(),
        code: None,
    });

    assert!(
        enc.transcode(&AgentEvent::TextDelta { delta: "a".into() })
            .is_empty()
    );
    assert!(
        enc.transcode(&AgentEvent::ToolCallDone {
            id: "c".into(),
            message_id: "m".into(),
            result: ToolResult::success("x", json!(null)),
            outcome: ToolCallOutcome::Succeeded,
        })
        .is_empty()
    );
    assert!(
        enc.transcode(&AgentEvent::RunFinish {
            thread_id: "t".into(),
            run_id: "r".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        })
        .is_empty()
    );
}

#[test]
fn step_events_present() {
    let mut enc = AcpEncoder::new();

    assert!(
        enc.transcode(&AgentEvent::StepStart {
            message_id: "step_1".into()
        })
        .is_empty()
    );
    assert!(enc.transcode(&AgentEvent::StepEnd).is_empty());
    assert!(
        enc.transcode(&AgentEvent::StepStart {
            message_id: "step_2".into()
        })
        .is_empty()
    );
    assert!(enc.transcode(&AgentEvent::StepEnd).is_empty());
}

#[test]
fn reasoning_delta_forwarded() {
    let mut enc = AcpEncoder::new();

    let ev1 = enc.transcode(&AgentEvent::ReasoningDelta {
        delta: "Let me think about this...".into(),
    });
    assert_eq!(ev1.len(), 1);
    assert_eq!(
        ev1[0],
        AcpEvent::agent_thought("Let me think about this...")
    );

    let ev2 = enc.transcode(&AgentEvent::ReasoningDelta {
        delta: " I should check the database.".into(),
    });
    assert_eq!(ev2.len(), 1);
    assert_eq!(
        ev2[0],
        AcpEvent::agent_thought(" I should check the database.")
    );
}

// ============================================================================
// Edge Cases
// ============================================================================

#[test]
fn unicode_preserved() {
    let mut enc = AcpEncoder::new();

    let ev = enc.transcode(&AgentEvent::TextDelta {
        delta: "Hello 世界! 🌍 Ñoño café résumé".into(),
    });
    assert_eq!(
        ev[0],
        AcpEvent::agent_message("Hello 世界! 🌍 Ñoño café résumé")
    );

    // Tool args with unicode
    let ev = enc.transcode(&AgentEvent::ToolCallReady {
        id: "c_unicode".into(),
        name: "translate".into(),
        arguments: json!({"text": "日本語テスト", "target": "en"}),
    });
    match &ev[0] {
        AcpEvent::SessionUpdate(params) => {
            let raw_input = params
                .tool_call_update
                .as_ref()
                .unwrap()
                .raw_input
                .as_ref()
                .unwrap();
            assert_eq!(raw_input["text"], "日本語テスト");
        }
        other => panic!("expected tool_call_update, got: {other:?}"),
    }

    let ev = enc.transcode(&AgentEvent::ReasoningDelta {
        delta: "思考中…".into(),
    });
    assert_eq!(ev[0], AcpEvent::agent_thought("思考中…"));
}

#[test]
fn large_payload_handled() {
    let mut enc = AcpEncoder::new();

    let large_text = "x".repeat(100_000);
    let ev = enc.transcode(&AgentEvent::TextDelta {
        delta: large_text.clone(),
    });
    assert_eq!(ev.len(), 1);
    match &ev[0] {
        AcpEvent::SessionUpdate(params) => {
            assert_eq!(params.agent_message_chunk.as_ref().unwrap().len(), 100_000);
        }
        other => panic!("expected agent_message, got: {other:?}"),
    }

    let large_array: Vec<serde_json::Value> = (0..10_000).map(|i| json!({"id": i})).collect();
    let ev = enc.transcode(&AgentEvent::ToolCallDone {
        id: "c_large".into(),
        message_id: "m_large".into(),
        result: ToolResult::success("big_query", json!(large_array)),
        outcome: ToolCallOutcome::Succeeded,
    });
    assert_eq!(ev.len(), 1);
    match &ev[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.status, ToolCallStatus::Completed);
            assert!(update.raw_output.is_some());
        }
        other => panic!("expected tool_call_update, got: {other:?}"),
    }
}

#[test]
fn special_characters_in_tool_result() {
    let mut enc = AcpEncoder::new();
    let special_content = json!({
        "output": "line1\nline2\ttab\r\nwindows",
        "path": "C:\\Users\\test\\file.txt",
        "html": "<script>alert('xss')</script>",
        "quotes": "He said \"hello\" and 'goodbye'",
        "backslash": "\\\\server\\share",
        "null_byte": "before\u{0000}after",
    });
    let ev = enc.transcode(&AgentEvent::ToolCallDone {
        id: "c_special".into(),
        message_id: "m_special".into(),
        result: ToolResult::success("shell", special_content.clone()),
        outcome: ToolCallOutcome::Succeeded,
    });
    assert_eq!(ev.len(), 1);
    match &ev[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.status, ToolCallStatus::Completed);
            let json_str = serde_json::to_string(&ev[0]).unwrap();
            let _restored: AcpEvent = serde_json::from_str(&json_str).unwrap();
        }
        other => panic!("expected tool_call_update, got: {other:?}"),
    }
}

#[test]
fn provider_error_produces_error() {
    let mut enc = AcpEncoder::new();

    let ev = enc.transcode(&AgentEvent::Error {
        message: "rate limit exceeded".into(),
        code: Some("429".into()),
    });
    assert_eq!(ev.len(), 1);
    assert_eq!(
        ev[0],
        AcpEvent::error("rate limit exceeded", Some("429".into()))
    );

    assert!(
        enc.transcode(&AgentEvent::Error {
            message: "another error".into(),
            code: None,
        })
        .is_empty()
    );
}

// ============================================================================
// Tool call kind inference
// ============================================================================

#[test]
fn tool_call_kind_inferred_from_name() {
    let mut enc = AcpEncoder::new();

    let check = |enc: &mut AcpEncoder, name: &str, expected_kind: ToolCallKind| {
        let ev = enc.on_agent_event(&AgentEvent::ToolCallReady {
            id: format!("c_{name}"),
            name: name.into(),
            arguments: json!({}),
        });
        match &ev[0] {
            AcpEvent::SessionUpdate(params) => {
                let update = params.tool_call_update.as_ref().unwrap();
                assert_eq!(
                    update.kind,
                    Some(expected_kind),
                    "tool '{name}' should have correct kind"
                );
            }
            other => panic!("expected SessionUpdate for '{name}', got: {other:?}"),
        }
    };

    check(&mut enc, "read_file", ToolCallKind::Read);
    // Reset encoder to avoid terminal guard issues
    let mut enc = AcpEncoder::new();
    check(&mut enc, "edit_file", ToolCallKind::Edit);
    let mut enc = AcpEncoder::new();
    check(&mut enc, "bash", ToolCallKind::Execute);
    let mut enc = AcpEncoder::new();
    check(&mut enc, "search", ToolCallKind::Search);
    let mut enc = AcpEncoder::new();
    check(&mut enc, "http_fetch", ToolCallKind::Fetch);
    let mut enc = AcpEncoder::new();
    check(&mut enc, "think", ToolCallKind::Think);
    let mut enc = AcpEncoder::new();
    check(&mut enc, "custom_tool", ToolCallKind::Other);
}

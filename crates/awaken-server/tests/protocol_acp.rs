//! ACP encoder contract tests — migrated from tirea-protocol-acp.
//!
//! Validates event mapping, termination reason mapping, permission flow,
//! state snapshot/delta visibility, tool call lifecycle, and terminal guard.

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::lifecycle::{StoppedReason, TerminationReason};
use awaken_contract::contract::suspension::ToolCallOutcome;
use awaken_contract::contract::tool::ToolResult;
use awaken_contract::contract::transport::Transcoder;
use awaken_server::protocols::acp::encoder::{AcpEncoder, AcpEvent, RequestPermissionParams};
use awaken_server::protocols::acp::types::{PermissionOption, StopReason, ToolCallStatus};
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
    assert_eq!(
        ev[0],
        AcpEvent::tool_call("call_1", "search", json!({"q": "rust"}))
    );

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
            assert_eq!(update.id, "call_1");
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
fn suspended_maps_to_suspended() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Suspended,
    });
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::Suspended)]);
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
    assert_eq!(ev[1], AcpEvent::finished(StopReason::Error));
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
fn blocked_maps_to_error() {
    let mut enc = AcpEncoder::new();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::Blocked("unsafe tool".into()),
    });
    assert_eq!(ev, vec![AcpEvent::finished(StopReason::Error)]);
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
fn tool_call_done_error_maps_to_errored() {
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
            assert_eq!(update.status, ToolCallStatus::Errored);
            assert_eq!(update.error.as_deref(), Some("backend failure"));
        }
        other => panic!("expected errored update, got: {other:?}"),
    }
}

// ============================================================================
// Permission flow (PermissionConfirm tool)
// ============================================================================

#[test]
fn permission_confirm_tool_emits_request_permission() {
    let mut enc = AcpEncoder::new();
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
        "should emit tool_call + request_permission"
    );

    assert_eq!(
        events[0],
        AcpEvent::tool_call(
            "fc_perm_1",
            "PermissionConfirm",
            json!({
                "tool_name": "bash",
                "tool_args": {"command": "rm -rf /tmp/test"}
            })
        )
    );

    match &events[1] {
        AcpEvent::RequestPermission(params) => {
            assert_eq!(params.tool_call_id, "fc_perm_1");
            assert_eq!(params.tool_name, "bash");
            assert_eq!(params.tool_args, json!({"command": "rm -rf /tmp/test"}));
            assert_eq!(
                params.options,
                vec![
                    PermissionOption::AllowOnce,
                    PermissionOption::AllowAlways,
                    PermissionOption::RejectOnce,
                    PermissionOption::RejectAlways,
                ]
            );
        }
        other => panic!("expected RequestPermission, got: {other:?}"),
    }
}

#[test]
fn permission_confirm_case_insensitive() {
    let mut enc = AcpEncoder::new();
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
    let mut enc = AcpEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "fc_3".into(),
        name: "PermissionConfirm".into(),
        arguments: json!({"tool_name": "echo"}),
    });
    assert_eq!(events.len(), 2);
    match &events[1] {
        AcpEvent::RequestPermission(RequestPermissionParams { tool_args, .. }) => {
            assert!(tool_args.is_null(), "missing tool_args should be null");
        }
        other => panic!("expected RequestPermission, got: {other:?}"),
    }
}

#[test]
fn permission_confirm_missing_tool_name_uses_unknown() {
    let mut enc = AcpEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "fc_4".into(),
        name: "PermissionConfirm".into(),
        arguments: json!({"tool_args": {"x": 1}}),
    });
    assert_eq!(events.len(), 2);
    match &events[1] {
        AcpEvent::RequestPermission(RequestPermissionParams { tool_name, .. }) => {
            assert_eq!(tool_name, "unknown");
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
            assert_eq!(update.id, "fc_perm_1");
            assert_eq!(update.status, ToolCallStatus::Completed);
            assert!(update.result.is_some());
        }
        other => panic!("expected completed update, got: {other:?}"),
    }
}

#[test]
fn denied_resolution_maps_to_denied_status() {
    let mut enc = AcpEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
        target_id: "fc_perm_1".into(),
        result: json!({"approved": false, "reason": "user rejected"}),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.id, "fc_perm_1");
            assert_eq!(update.status, ToolCallStatus::Denied);
            assert!(update.result.is_none());
            assert!(update.error.is_none());
        }
        other => panic!("expected denied update, got: {other:?}"),
    }
}

#[test]
fn error_resolution_maps_to_errored_status() {
    let mut enc = AcpEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
        target_id: "fc_perm_1".into(),
        result: json!({"error": "frontend validation failed"}),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        AcpEvent::SessionUpdate(params) => {
            let update = params.tool_call_update.as_ref().unwrap();
            assert_eq!(update.id, "fc_perm_1");
            assert_eq!(update.status, ToolCallStatus::Errored);
            assert_eq!(update.error.as_deref(), Some("frontend validation failed"));
        }
        other => panic!("expected errored update, got: {other:?}"),
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
    let event = AcpEvent::request_permission("fc_1", "bash", json!({"command": "rm"}));
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
        ToolCallStatus::InProgress,
        ToolCallStatus::Completed,
        ToolCallStatus::Denied,
        ToolCallStatus::Errored,
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
        StopReason::Cancelled,
        StopReason::Error,
        StopReason::Suspended,
    ] {
        let json = serde_json::to_string(&reason).unwrap();
        let parsed: StopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, reason);
    }
}

#[test]
fn permission_option_serde_roundtrip() {
    for opt in [
        PermissionOption::AllowOnce,
        PermissionOption::AllowAlways,
        PermissionOption::RejectOnce,
        PermissionOption::RejectAlways,
    ] {
        let json = serde_json::to_string(&opt).unwrap();
        let parsed: PermissionOption = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, opt);
    }
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

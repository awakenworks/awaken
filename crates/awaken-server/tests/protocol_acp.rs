//! ACP encoder contract tests using `agent-client-protocol-schema` types.

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::lifecycle::{StoppedReason, TerminationReason};
use awaken_contract::contract::suspension::{
    PendingToolCall, SuspendTicket, Suspension, ToolCallOutcome, ToolCallResumeMode,
};
use awaken_contract::contract::tool::ToolResult;
use awaken_contract::contract::transport::Transcoder;
use awaken_server::protocols::acp::encoder::{AcpEncoder, AcpOutput};
use awaken_server::protocols::acp::types::{SessionUpdate, StopReason, ToolCallStatus, ToolKind};
use serde_json::json;

fn enc() -> AcpEncoder {
    AcpEncoder::new().with_session_id("sess_test")
}

fn assert_notif(o: &AcpOutput) -> &agent_client_protocol_schema::SessionNotification {
    match o {
        AcpOutput::Notification(n) => n,
        other => panic!("expected Notification, got: {other:?}"),
    }
}

fn assert_finished(o: &AcpOutput) -> StopReason {
    match o {
        AcpOutput::Finished(r) => *r,
        other => panic!("expected Finished, got: {other:?}"),
    }
}

// ── Transcoder trait ────────────────────────────────────────────────

#[test]
fn transcoder_trait_delegates() {
    let mut enc = enc();
    let events = enc.transcode(&AgentEvent::TextDelta { delta: "hi".into() });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], AcpOutput::Notification(_)));
}

// ── Full lifecycle ──────────────────────────────────────────────────

#[test]
fn full_lifecycle_text_tool_text_finish() {
    let mut enc = enc();

    assert!(
        enc.transcode(&AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: None,
        })
        .is_empty()
    );

    assert!(
        enc.transcode(&AgentEvent::StepStart {
            message_id: "msg_1".into()
        })
        .is_empty()
    );

    let ev = enc.transcode(&AgentEvent::TextDelta {
        delta: "Hello ".into(),
    });
    assert!(matches!(
        &assert_notif(&ev[0]).update,
        SessionUpdate::AgentMessageChunk(_)
    ));

    assert!(
        enc.transcode(&AgentEvent::ToolCallStart {
            id: "c1".into(),
            name: "search".into()
        })
        .is_empty()
    );

    let ev = enc.transcode(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "search".into(),
        arguments: json!({"q": "rust"}),
    });
    match &assert_notif(&ev[0]).update {
        SessionUpdate::ToolCall(tc) => {
            assert_eq!(tc.status, ToolCallStatus::Pending);
            assert_eq!(tc.kind, ToolKind::Search);
        }
        other => panic!("expected ToolCall, got: {other:?}"),
    }

    let ev = enc.transcode(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::success("search", json!({"results": [1, 2]})),
        outcome: ToolCallOutcome::Succeeded,
    });
    match &assert_notif(&ev[0]).update {
        SessionUpdate::ToolCallUpdate(u) => {
            assert_eq!(u.fields.status, Some(ToolCallStatus::Completed))
        }
        other => panic!("expected ToolCallUpdate, got: {other:?}"),
    }

    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    assert_eq!(assert_finished(&ev[0]), StopReason::EndTurn);
}

// ── Terminal guard ──────────────────────────────────────────────────

#[test]
fn events_after_run_finish_suppressed() {
    let mut enc = enc();
    enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t".into(),
        run_id: "r".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    assert!(
        enc.transcode(&AgentEvent::TextDelta {
            delta: "late".into()
        })
        .is_empty()
    );
}

#[test]
fn events_after_error_suppressed() {
    let mut enc = enc();
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

// ── Termination mapping ─────────────────────────────────────────────

#[test]
fn cancelled_maps_to_cancelled() {
    let mut enc = enc();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t".into(),
        run_id: "r".into(),
        result: None,
        termination: TerminationReason::Cancelled,
    });
    assert_eq!(assert_finished(&ev[0]), StopReason::Cancelled);
}

#[test]
fn suspended_does_not_emit_terminal_output() {
    let mut enc = enc();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t".into(),
        run_id: "r".into(),
        result: None,
        termination: TerminationReason::Suspended,
    });
    assert!(ev.is_empty());
    let follow_up = enc.transcode(&AgentEvent::TextDelta {
        delta: "still-running".into(),
    });
    assert_eq!(follow_up.len(), 1);
}

#[test]
fn error_termination_emits_error_then_finished() {
    let mut enc = enc();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t".into(),
        run_id: "r".into(),
        result: None,
        termination: TerminationReason::Error("boom".into()),
    });
    assert_eq!(ev.len(), 2);
    assert!(matches!(&ev[0], AcpOutput::Error { .. }));
    assert_eq!(assert_finished(&ev[1]), StopReason::EndTurn);
}

#[test]
fn blocked_maps_to_refusal() {
    let mut enc = enc();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t".into(),
        run_id: "r".into(),
        result: None,
        termination: TerminationReason::Blocked("unsafe".into()),
    });
    assert_eq!(assert_finished(&ev[0]), StopReason::Refusal);
}

#[test]
fn max_rounds_maps_to_max_tokens() {
    let mut enc = enc();
    let ev = enc.transcode(&AgentEvent::RunFinish {
        thread_id: "t".into(),
        run_id: "r".into(),
        result: None,
        termination: TerminationReason::Stopped(StoppedReason::new("max_rounds_reached")),
    });
    assert_eq!(assert_finished(&ev[0]), StopReason::MaxTokens);
}

// ── Tool calls ──────────────────────────────────────────────────────

#[test]
fn tool_call_done_error_maps_to_failed() {
    let mut enc = enc();
    let ev = enc.transcode(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::error("search", "backend failure"),
        outcome: ToolCallOutcome::Failed,
    });
    match &assert_notif(&ev[0]).update {
        SessionUpdate::ToolCallUpdate(u) => {
            assert_eq!(u.fields.status, Some(ToolCallStatus::Failed))
        }
        other => panic!("expected ToolCallUpdate, got: {other:?}"),
    }
}

#[test]
fn tool_call_kind_inferred() {
    let check = |name: &str, expected: ToolKind| {
        let mut enc = enc();
        let ev = enc.on_agent_event(&AgentEvent::ToolCallReady {
            id: format!("c_{name}"),
            name: name.into(),
            arguments: json!({}),
        });
        match &assert_notif(&ev[0]).update {
            SessionUpdate::ToolCall(tc) => assert_eq!(tc.kind, expected, "tool '{name}'"),
            other => panic!("expected ToolCall for '{name}', got: {other:?}"),
        }
    };
    check("read_file", ToolKind::Read);
    check("edit_file", ToolKind::Edit);
    check("bash", ToolKind::Execute);
    check("search", ToolKind::Search);
    check("http_fetch", ToolKind::Fetch);
    check("think", ToolKind::Think);
    check("custom_tool", ToolKind::Other);
}

// ── Permission flow ─────────────────────────────────────────────────

#[test]
fn suspended_permission_tool_emits_request_permission() {
    let mut enc = enc();
    let ready_events = enc.on_agent_event(&AgentEvent::ToolCallReady {
        id: "fc_1".into(),
        name: "bash".into(),
        arguments: json!({"cmd": "ls"}),
    });
    assert_eq!(ready_events.len(), 1);

    let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
        id: "fc_1".into(),
        message_id: "m1".into(),
        result: ToolResult::suspended_with(
            "bash",
            "awaiting approval",
            SuspendTicket::new(
                Suspension {
                    action: "tool:PermissionConfirm".into(),
                    ..Default::default()
                },
                PendingToolCall::new("perm_fc_1", "permission_confirm", json!({"cmd": "ls"})),
                ToolCallResumeMode::ReplayToolCall,
            ),
        ),
        outcome: ToolCallOutcome::Suspended,
    });
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], AcpOutput::PermissionRequest(_)));
}

#[test]
fn approved_resolution_maps_to_completed() {
    let mut enc = enc();
    let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
        target_id: "fc_1".into(),
        result: json!({"approved": true}),
    });
    match &assert_notif(&events[0]).update {
        SessionUpdate::ToolCallUpdate(u) => {
            assert_eq!(u.fields.status, Some(ToolCallStatus::Completed))
        }
        other => panic!("expected completed, got: {other:?}"),
    }
}

#[test]
fn denied_resolution_maps_to_failed() {
    let mut enc = enc();
    let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
        target_id: "fc_1".into(),
        result: json!({"approved": false}),
    });
    match &assert_notif(&events[0]).update {
        SessionUpdate::ToolCallUpdate(u) => {
            assert_eq!(u.fields.status, Some(ToolCallStatus::Failed))
        }
        other => panic!("expected failed, got: {other:?}"),
    }
}

// ── Serde roundtrips ────────────────────────────────────────────────

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

// ── Error event ─────────────────────────────────────────────────────

#[test]
fn error_event_sets_terminal_guard() {
    let mut enc = enc();
    let ev = enc.on_agent_event(&AgentEvent::Error {
        message: "fatal".into(),
        code: Some("E001".into()),
    });
    assert!(matches!(&ev[0], AcpOutput::Error { .. }));
    assert!(
        enc.on_agent_event(&AgentEvent::TextDelta { delta: "x".into() })
            .is_empty()
    );
}

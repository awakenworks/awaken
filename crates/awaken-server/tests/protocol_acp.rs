//! ACP encoder public API smoke tests.

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::lifecycle::TerminationReason;
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

fn assert_notification(output: &AcpOutput) -> &agent_client_protocol_schema::SessionNotification {
    match output {
        AcpOutput::Notification(notification) => notification,
        other => panic!("expected Notification, got: {other:?}"),
    }
}

#[test]
fn encoder_public_lifecycle_smoke() {
    let mut encoder = enc();

    let message = encoder.transcode(&AgentEvent::TextDelta {
        delta: "Hello ".into(),
    });
    assert!(matches!(
        &assert_notification(&message[0]).update,
        SessionUpdate::AgentMessageChunk(_)
    ));

    let tool_call = encoder.transcode(&AgentEvent::ToolCallReady {
        id: "c1".into(),
        name: "search".into(),
        arguments: json!({"q": "rust"}),
    });
    match &assert_notification(&tool_call[0]).update {
        SessionUpdate::ToolCall(call) => {
            assert_eq!(call.status, ToolCallStatus::Pending);
            assert_eq!(call.kind, ToolKind::Search);
        }
        other => panic!("expected ToolCall, got: {other:?}"),
    }

    let tool_done = encoder.transcode(&AgentEvent::ToolCallDone {
        id: "c1".into(),
        message_id: "m1".into(),
        result: ToolResult::success("search", json!({"results": [1, 2]})),
        outcome: ToolCallOutcome::Succeeded,
    });
    match &assert_notification(&tool_done[0]).update {
        SessionUpdate::ToolCallUpdate(update) => {
            assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
        }
        other => panic!("expected ToolCallUpdate, got: {other:?}"),
    }

    let finish = encoder.transcode(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    assert_eq!(finish.len(), 1);
    match &finish[0] {
        AcpOutput::Finished(reason) => assert_eq!(*reason, StopReason::EndTurn),
        other => panic!("expected Finished, got: {other:?}"),
    }
}

#[test]
fn encoder_public_permission_request_smoke() {
    let mut encoder = enc();
    let ready = encoder.on_agent_event(&AgentEvent::ToolCallReady {
        id: "fc_1".into(),
        name: "bash".into(),
        arguments: json!({"cmd": "ls"}),
    });
    assert_eq!(ready.len(), 1);

    let suspended = encoder.on_agent_event(&AgentEvent::ToolCallDone {
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

    assert_eq!(suspended.len(), 1);
    assert!(matches!(&suspended[0], AcpOutput::PermissionRequest(_)));
}

#[test]
fn encoder_rejects_unsupported_suspended_tool_actions() {
    let mut encoder = enc();
    let ready = encoder.on_agent_event(&AgentEvent::ToolCallReady {
        id: "fc_1".into(),
        name: "ask_user".into(),
        arguments: json!({"question": "What color?"}),
    });
    assert_eq!(ready.len(), 1);

    let suspended = encoder.on_agent_event(&AgentEvent::ToolCallDone {
        id: "fc_1".into(),
        message_id: "m1".into(),
        result: ToolResult::suspended_with(
            "ask_user",
            "awaiting frontend handling",
            SuspendTicket::new(
                Suspension {
                    action: "tool:ask_user".into(),
                    ..Default::default()
                },
                PendingToolCall::new(
                    "suspend_fc_1",
                    "ask_user",
                    json!({"question": "What color?"}),
                ),
                ToolCallResumeMode::UseDecisionAsToolResult,
            ),
        ),
        outcome: ToolCallOutcome::Suspended,
    });

    assert_eq!(suspended.len(), 1);
    match &suspended[0] {
        AcpOutput::Error { message, .. } => {
            assert!(
                message.contains("only supports suspended tool action 'tool:PermissionConfirm'")
            );
        }
        other => panic!("expected Error, got: {other:?}"),
    }
}

#[test]
fn encoder_terminal_guard_smoke() {
    let mut encoder = enc();
    let finish = encoder.on_agent_event(&AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    assert!(matches!(
        finish[0],
        AcpOutput::Finished(StopReason::EndTurn)
    ));

    let late = encoder.on_agent_event(&AgentEvent::TextDelta {
        delta: "ignored".into(),
    });
    assert!(late.is_empty());
}

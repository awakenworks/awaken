//! Permission suspension → ACP request_permission → resolution tests.

use serde_json::json;
use tirea_contract::AgentEvent;
use tirea_protocol_acp::{
    AcpEncoder, AcpEvent, PermissionOption, RequestPermissionParams, ToolCallStatus,
};

// ==========================================================================
// PermissionConfirm produces RequestPermission
// ==========================================================================

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

    // First: the tool_call event
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

    // Second: the request_permission event
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
    assert!(matches!(events[1], AcpEvent::RequestPermission(_)));
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
    assert!(matches!(events[0], AcpEvent::SessionUpdate(_)));
}

// ==========================================================================
// Approved resolution
// ==========================================================================

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

// ==========================================================================
// Denied resolution
// ==========================================================================

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

// ==========================================================================
// Error resolution
// ==========================================================================

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

// ==========================================================================
// Permission tool with missing tool_args
// ==========================================================================

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

// ==========================================================================
// Permission tool with missing tool_name
// ==========================================================================

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

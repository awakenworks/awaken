//! Tests verifying permission_overrides visibility in state snapshots and deltas.

use serde_json::json;
use tirea_contract::AgentEvent;
use tirea_protocol_acp::{AcpEncoder, AcpEvent};

// ==========================================================================
// State snapshot with permission_overrides
// ==========================================================================

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
            let overrides = &snap["permission_overrides"];
            assert_eq!(overrides["allow_tools"][0], "bash");
            assert_eq!(overrides["allow_tools"][1], "search");
        }
        other => panic!("expected state_snapshot, got: {other:?}"),
    }
}

// ==========================================================================
// State delta reflecting override changes
// ==========================================================================

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
            assert_eq!(d, &delta);
            assert_eq!(d[0]["op"], "remove");
        }
        other => panic!("expected state_delta, got: {other:?}"),
    }
}

// ==========================================================================
// Cleanup removes overrides from snapshot
// ==========================================================================

#[test]
fn state_snapshot_after_cleanup_has_no_overrides() {
    let mut enc = AcpEncoder::new();

    // First snapshot with overrides
    let _ = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({
            "permission_overrides": {"allow_tools": ["bash"]},
            "other": "data"
        }),
    });

    // Second snapshot after run cleanup (overrides removed)
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({
            "other": "data"
        }),
    });

    assert_eq!(events.len(), 1);
    match &events[0] {
        AcpEvent::SessionUpdate(params) => {
            let snap = params
                .state_snapshot
                .as_ref()
                .expect("expected state_snapshot");
            assert!(
                snap.get("permission_overrides").is_none(),
                "overrides should be absent after cleanup"
            );
        }
        other => panic!("expected state_snapshot, got: {other:?}"),
    }
}

// ==========================================================================
// Multiple sequential deltas
// ==========================================================================

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

    let ev1 = enc.on_agent_event(&AgentEvent::StateDelta {
        delta: delta1.clone(),
    });
    let ev2 = enc.on_agent_event(&AgentEvent::StateDelta {
        delta: delta2.clone(),
    });

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

// ==========================================================================
// Empty state snapshot
// ==========================================================================

#[test]
fn empty_state_snapshot_forwarded() {
    let mut enc = AcpEncoder::new();
    let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
        snapshot: json!({}),
    });
    assert_eq!(events.len(), 1);
    assert_eq!(events[0], AcpEvent::state_snapshot(json!({})));
}

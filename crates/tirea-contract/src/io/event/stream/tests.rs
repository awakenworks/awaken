use super::AgentEvent;
use crate::runtime::tool_call::ToolResult;
use crate::runtime::TerminationReason;
use serde_json::{json, Value};

#[test]
fn reasoning_delta_roundtrip() {
    let event = AgentEvent::ReasoningDelta {
        delta: "think".to_string(),
    };
    let raw = serde_json::to_string(&event).expect("serialize reasoning delta");
    let restored: AgentEvent = serde_json::from_str(&raw).expect("deserialize reasoning delta");
    assert!(matches!(restored, AgentEvent::ReasoningDelta { delta } if delta == "think"));
}

#[test]
fn reasoning_encrypted_value_roundtrip() {
    let event = AgentEvent::ReasoningEncryptedValue {
        encrypted_value: "opaque".to_string(),
    };
    let raw = serde_json::to_string(&event).expect("serialize reasoning encrypted value");
    let restored: AgentEvent =
        serde_json::from_str(&raw).expect("deserialize reasoning encrypted value");
    assert!(matches!(
        restored,
        AgentEvent::ReasoningEncryptedValue { encrypted_value } if encrypted_value == "opaque"
    ));
}

#[test]
fn envelope_identity_run_start_wire_format() {
    let event = AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: Some("parent".into()),
    };
    let wire: Value = serde_json::to_value(&event).unwrap();
    // run_id and thread_id appear at envelope top-level, not in data
    assert_eq!(wire["type"], "run_start");
    assert_eq!(wire["run_id"], "r1");
    assert_eq!(wire["thread_id"], "t1");
    assert_eq!(wire["data"]["parent_run_id"], "parent");
    // data should NOT contain run_id/thread_id
    assert!(wire["data"].get("run_id").is_none());
    assert!(wire["data"].get("thread_id").is_none());

    let restored: AgentEvent = serde_json::from_value(wire).unwrap();
    assert!(matches!(
        restored,
        AgentEvent::RunStart { thread_id, run_id, parent_run_id }
            if thread_id == "t1" && run_id == "r1" && parent_run_id.as_deref() == Some("parent")
    ));
}

#[test]
fn envelope_identity_run_finish_wire_format() {
    let event = AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: Some(json!({"response": "hello"})),
        termination: TerminationReason::NaturalEnd,
    };
    let wire: Value = serde_json::to_value(&event).unwrap();
    assert_eq!(wire["type"], "run_finish");
    assert_eq!(wire["run_id"], "r1");
    assert_eq!(wire["thread_id"], "t1");
    assert_eq!(wire["data"]["result"]["response"], "hello");
    assert_eq!(wire["data"]["termination"]["type"], "natural_end");

    let restored: AgentEvent = serde_json::from_value(wire).unwrap();
    assert!(matches!(restored, AgentEvent::RunFinish { run_id, .. } if run_id == "r1"));
}

#[test]
fn wire_extra_tool_call_done_roundtrip() {
    let event = AgentEvent::ToolCallDone {
        id: "call_1".into(),
        result: ToolResult::success("calc", json!(42)),
        patch: None,
        message_id: "msg_1".into(),
    };
    let wire: Value = serde_json::to_value(&event).unwrap();
    // Wire-only `outcome` field should appear in serialized data
    assert!(wire["data"].get("outcome").is_some());
    assert_eq!(wire["data"]["outcome"], "succeeded");

    let restored: AgentEvent = serde_json::from_value(wire).unwrap();
    assert!(matches!(
        restored,
        AgentEvent::ToolCallDone { id, message_id, .. }
            if id == "call_1" && message_id == "msg_1"
    ));
}

#[test]
fn no_data_step_end_roundtrip() {
    let event = AgentEvent::StepEnd;
    let wire: Value = serde_json::to_value(&event).unwrap();
    assert_eq!(wire["type"], "step_end");
    // data should be absent (empty object collapses to None)
    assert!(wire.get("data").is_none());

    let restored: AgentEvent = serde_json::from_value(wire).unwrap();
    assert!(matches!(restored, AgentEvent::StepEnd));
}

#[test]
fn standard_activity_snapshot_roundtrip() {
    let event = AgentEvent::ActivitySnapshot {
        message_id: "msg_1".into(),
        activity_type: "thinking".into(),
        content: json!({"text": "processing"}),
        replace: Some(true),
    };
    let wire: Value = serde_json::to_value(&event).unwrap();
    assert_eq!(wire["type"], "activity_snapshot");
    assert_eq!(wire["data"]["activity_type"], "thinking");
    assert_eq!(wire["data"]["replace"], true);

    let restored: AgentEvent = serde_json::from_value(wire).unwrap();
    assert!(matches!(
        restored,
        AgentEvent::ActivitySnapshot {
            replace: Some(true),
            ..
        }
    ));
}

#[test]
fn standard_activity_snapshot_omits_null_replace() {
    let event = AgentEvent::ActivitySnapshot {
        message_id: "msg_1".into(),
        activity_type: "thinking".into(),
        content: json!({}),
        replace: None,
    };
    let wire: Value = serde_json::to_value(&event).unwrap();
    // skip_serializing_if should omit None replace
    assert!(wire["data"].get("replace").is_none());
}

#[test]
fn all_event_types_roundtrip() {
    let events: Vec<AgentEvent> = vec![
        AgentEvent::RunStart {
            thread_id: "t".into(),
            run_id: "r".into(),
            parent_run_id: None,
        },
        AgentEvent::RunFinish {
            thread_id: "t".into(),
            run_id: "r".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        },
        AgentEvent::TextDelta { delta: "hi".into() },
        AgentEvent::ReasoningDelta {
            delta: "think".into(),
        },
        AgentEvent::ReasoningEncryptedValue {
            encrypted_value: "enc".into(),
        },
        AgentEvent::ToolCallStart {
            id: "c".into(),
            name: "t".into(),
        },
        AgentEvent::ToolCallDelta {
            id: "c".into(),
            args_delta: "{}".into(),
        },
        AgentEvent::ToolCallReady {
            id: "c".into(),
            name: "t".into(),
            arguments: json!({}),
        },
        AgentEvent::ToolCallDone {
            id: "c".into(),
            result: ToolResult::success("t", json!(null)),
            patch: None,
            message_id: "m".into(),
        },
        AgentEvent::StepStart {
            message_id: "m".into(),
        },
        AgentEvent::StepEnd,
        AgentEvent::InferenceComplete {
            model: "gpt".into(),
            usage: None,
            duration_ms: 100,
        },
        AgentEvent::StateSnapshot {
            snapshot: json!({}),
        },
        AgentEvent::StateDelta {
            delta: vec![json!({})],
        },
        AgentEvent::MessagesSnapshot {
            messages: vec![json!({})],
        },
        AgentEvent::ActivitySnapshot {
            message_id: "m".into(),
            activity_type: "a".into(),
            content: json!({}),
            replace: None,
        },
        AgentEvent::ActivityDelta {
            message_id: "m".into(),
            activity_type: "a".into(),
            patch: vec![],
        },
        AgentEvent::ToolCallResumed {
            target_id: "t".into(),
            result: json!({}),
        },
        AgentEvent::Error {
            message: "oops".into(),
            code: Some("LLM_ERROR".into()),
        },
    ];

    for event in &events {
        let wire = serde_json::to_value(event).expect("serialize");
        let _restored: AgentEvent = serde_json::from_value(wire).expect("deserialize");
    }
}

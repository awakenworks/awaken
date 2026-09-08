use super::*;

#[test]
fn durable_reference_and_resume_projection_fail_closed() {
    // Constraints/invariants: the current fenced attempt and committed context are
    // authoritative; remote protocol state cannot become a parallel Run or transcript truth.
    // Coverage rationale: `durable reference and resume projection fail closed` is one
    // independent branch selecting `all output, state, side-effect, error, and terminal
    // assertions below hold together`; a multi-row decision table is not applicable, and
    // sibling tests own alternate causes.
    // Causes: C1 exact versioned Accepted operation; C2 flat, missing,
    // empty, unknown, or unsupported-version state; C3 endpoint mismatch;
    // C4 concrete resume result; C5 native Continue. Effects: E1 typed
    // decode; E2/E3 rejection; E4 payload; E5 no payload.
    // Rules: C1=>E1; C2=>E2; C1+C3=>E3; C4=>E4; C5=>E5.
    let reference = TaskReference {
        endpoint: "http://remote.invalid".into(),
        task_id: "task-7".into(),
        context_id: "ctx-7".into(),
    };
    let StateCommand {
        action: awaken_agent_contract::agent::state::Action::Set(valid),
        ..
    } = accepted_reference_state(&reference).unwrap()
    else {
        panic!("Accepted fixture writes one Set")
    };
    let accepted = match decode_remote_operation(&valid).unwrap() {
        RemoteOperationState::Accepted(accepted) => accepted,
        RemoteOperationState::Prepared(_) => panic!("accepted state decoded as Prepared"),
    };
    assert_eq!(accepted.reference, reference);
    assert_eq!(accepted.intent.context_id, "ctx-7");
    assert!(
        decode_remote_operation(&serde_json::json!({
            "endpoint": "http://remote.invalid",
            "task_id": "task-7",
            "context_id": "ctx-7",
        }))
        .is_err(),
        "flat pre-cutover state is not a second production schema"
    );
    for field in ["endpoint", "task_id", "context_id"] {
        let mut invalid = valid.clone();
        invalid["reference"].as_object_mut().unwrap().remove(field);
        assert!(
            decode_remote_operation(&invalid).is_err(),
            "missing {field}"
        );

        let mut empty = valid.clone();
        empty["reference"][field] = serde_json::Value::String(String::new());
        assert!(decode_remote_operation(&empty).is_err(), "empty {field}");
    }
    let mut unknown = valid.clone();
    unknown["unexpected"] = serde_json::Value::Bool(true);
    assert!(decode_remote_operation(&unknown).is_err(), "unknown field");
    let mut unsupported = valid.clone();
    unsupported["version"] = serde_json::json!(3);
    assert!(
        decode_remote_operation(&unsupported).is_err(),
        "unknown schema version"
    );
    assert!(matches!(
        remote_operation_decision(StoredRemoteOperationKind::Absent, false),
        RemoteOperationDecision::Fresh
    ));
    assert!(matches!(
        remote_operation_decision(StoredRemoteOperationKind::Prepared, true),
        RemoteOperationDecision::PreservePrepared
    ));
    assert!(matches!(
        remote_operation_decision(StoredRemoteOperationKind::Accepted, true),
        RemoteOperationDecision::Reattach
    ));
    assert!(matches!(
        remote_operation_decision(StoredRemoteOperationKind::Accepted, false),
        RemoteOperationDecision::RejectEndpointMismatch
    ));

    assert_eq!(
        resume_text(&ResumeResult::Input("input".into())).unwrap(),
        "input"
    );
    assert_eq!(
        resume_text(&ResumeResult::ToolResult(
            awaken_runtime_contract::tool::ToolOutput::ok("call", "tool")
        ))
        .unwrap(),
        "tool"
    );
    assert_eq!(resume_text(&ResumeResult::allow()).unwrap(), "allow");
    assert_eq!(resume_text(&ResumeResult::deny(None)).unwrap(), "deny");
    assert_eq!(
        resume_text(&ResumeResult::deny(Some("because".into()))).unwrap(),
        "because"
    );
    assert!(resume_text(&ResumeResult::Continue).is_err(), "C5/E5");
}

#[test]
fn remote_operation_identity_binds_every_send_coordinate_and_schema_phase() {
    // Causes: C1 exact endpoint/context/predecessor/message/input tuple;
    // C2 one coordinate changes; C3 persisted identity is tampered; C4 a
    // matching continuation task is accepted. Effects: E1 stable identity;
    // E2 distinct identity; E3 fail-closed decode; E4 versioned phase
    // changes from Prepared to Accepted while retaining the exact intent.
    //
    // | Rule | Coordinates | Persisted identity | Task | Effects |
    // |---|---|---|---|---|
    // | R1 | same | exact | - | E1 |
    // | R2 | one differs | exact | - | E2 |
    // | R3 | same | tampered | - | E3 |
    // | R4 | same | exact | matching | E4 |
    let intent = prepare_remote_operation(
        "http://remote.invalid",
        "ctx-a",
        Some("task-a"),
        "message-b",
        "answer",
    )
    .unwrap();
    assert_eq!(
        intent,
        prepare_remote_operation(
            "http://remote.invalid",
            "ctx-a",
            Some("task-a"),
            "message-b",
            "answer",
        )
        .unwrap(),
        "R1/E1"
    );
    for changed in [
        prepare_remote_operation(
            "http://other.invalid",
            "ctx-a",
            Some("task-a"),
            "message-b",
            "answer",
        )
        .unwrap(),
        prepare_remote_operation(
            "http://remote.invalid",
            "ctx-other",
            Some("task-a"),
            "message-b",
            "answer",
        )
        .unwrap(),
        prepare_remote_operation(
            "http://remote.invalid",
            "ctx-a",
            Some("task-other"),
            "message-b",
            "answer",
        )
        .unwrap(),
        prepare_remote_operation(
            "http://remote.invalid",
            "ctx-a",
            Some("task-a"),
            "message-other",
            "answer",
        )
        .unwrap(),
        prepare_remote_operation(
            "http://remote.invalid",
            "ctx-a",
            Some("task-a"),
            "message-b",
            "different answer",
        )
        .unwrap(),
    ] {
        assert_ne!(intent.operation_id, changed.operation_id, "R2/E2");
    }

    let prepared = prepared_operation_state(&intent).unwrap();
    let awaken_agent_contract::agent::state::Action::Set(mut persisted) = prepared.action else {
        panic!("Prepared is a Set")
    };
    assert_eq!(persisted["phase"], "prepared");
    assert_eq!(persisted["version"], 2);
    assert_eq!(
        decode_remote_operation(&persisted).unwrap(),
        RemoteOperationState::Prepared(intent.clone())
    );
    persisted["intent"]["context_id"] = serde_json::json!("tampered");
    assert!(decode_remote_operation(&persisted).is_err(), "R3/E3");

    let task: Task = serde_json::from_value(serde_json::json!({
        "kind": "task",
        "id": "task-b",
        "contextId": "ctx-a",
        "status": { "state": "working" }
    }))
    .unwrap();
    let accepted = accept_remote_operation(intent.clone(), &task).unwrap();
    let command = accepted_operation_state(&accepted).unwrap();
    let awaken_agent_contract::agent::state::Action::Set(persisted) = command.action else {
        panic!("Accepted is a Set")
    };
    assert_eq!(persisted["phase"], "accepted", "R4/E4");
    assert_eq!(persisted["version"], 2, "R4/E4");
    assert_eq!(
        decode_remote_operation(&persisted).unwrap(),
        RemoteOperationState::Accepted(accepted),
        "R4/E4"
    );
}

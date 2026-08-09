use super::*;

/// The only required create field is the agent; every other field defaults.
pub(in crate::state) fn bare_create_params() -> SessionCreateParams {
    serde_json::from_value(serde_json::json!({ "agent": "assistant" }))
        .expect("minimal create params deserialize")
}

#[tokio::test]
async fn externally_identified_application_session_follows_the_causal_decision_table() {
    // Cause graph: exact external identity + required application contribution
    // admits a preparing Control aggregate. Either absent cause fails before a
    // durable row is authored.
    //
    // | Rule | exact id | application required | Effect |
    // | E1 | non-empty | yes | create under exact id |
    // | E2 | empty | yes | reject, no row |
    // | E3 | non-empty | no | reject, no row |
    // | E4 | same id | same request | replay exact Session |
    // | E5 | same id | different request | reject conflict |
    let state = ManagedState::new_with_mcp(RehydrateFake::default());
    let required: SessionCreateParams = serde_json::from_value(serde_json::json!({
        "agent": "assistant",
        "application_contribution_required": true
    }))
    .unwrap();
    let created = state
        .create_application_session("flow/run-1", required.clone(), Some("workspace".into()))
        .await
        .expect("E1");
    assert_eq!(created.id, "flow/run-1", "E1 exact identity");
    assert_eq!(
        created.status,
        SessionStatus::Preparing,
        "E1 waits for contribution"
    );
    let replayed = state
        .create_application_session("flow/run-1", required.clone(), Some("workspace".into()))
        .await
        .expect("E4");
    assert_eq!(replayed.id, created.id, "E4 exact replay");

    let different: SessionCreateParams = serde_json::from_value(serde_json::json!({
        "agent": "other-agent",
        "application_contribution_required": true
    }))
    .unwrap();
    assert!(
        state
            .create_application_session("flow/run-1", different, Some("workspace".into()),)
            .await
            .is_err(),
        "E5"
    );

    assert!(
        state
            .create_application_session(" ", required, Some("workspace".into()))
            .await
            .is_err(),
        "E2"
    );
    assert!(
        state
            .create_application_session("flow/run-2", bare_create_params(), None)
            .await
            .is_err(),
        "E3"
    );
    assert!(
        state.application.session("flow/run-2").await.is_err(),
        "E3 no row"
    );
}

#[tokio::test]
async fn delete_broadcasts_session_deleted_then_removes_the_record() {
    let state = ManagedState::new_with_mcp(RehydrateFake::default());
    let id = state
        .create_session(bare_create_params(), None)
        .await
        .expect("create")
        .id;

    // Subscribe as an SSE client would, *before* the delete. A fresh session
    // has no committed events, so the stream tails live rather than ending on
    // a terminal backfill — the window in which `session.deleted` is observed.
    let (snapshot, mut rx) = state.stream_subscribe(&id).expect("subscribe");
    assert!(
        snapshot.is_empty(),
        "a fresh session has no committed events"
    );

    state.delete_session(&id).await.expect("delete");

    // The terminal frame reached the open stream before the record was dropped.
    match rx
        .try_recv()
        .expect("a frame was broadcast to the open stream")
    {
        StreamFrame::Committed(e) => assert_eq!(
            e.type_str(),
            "session.deleted",
            "the broadcast terminal frame is session.deleted"
        ),
        other => panic!("expected a committed session.deleted frame, got {other:?}"),
    }

    // And the record is gone: retrieve and events.list are now 404, by design
    // (delete removes the session; it does not tombstone it as archive does).
    assert!(
        matches!(state.get_session(&id), Err(StateError::NotFound)),
        "the deleted session is no longer retrievable"
    );
    assert!(
        matches!(
            state.list_events(&id, None, None, false),
            Err(StateError::NotFound)
        ),
        "events.list on a deleted session is a 404, not a replay"
    );
}

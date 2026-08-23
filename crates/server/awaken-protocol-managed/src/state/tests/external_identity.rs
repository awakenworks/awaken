use super::*;

/// Managed creation requires both the Agent and an explicit Environment.
pub(in crate::state) fn bare_create_params() -> SessionCreateParams {
    serde_json::from_value(serde_json::json!({
        "agent": "assistant",
        "environment_id": "env_local"
    }))
    .expect("minimal create params deserialize")
}

#[tokio::test]
async fn delete_broadcasts_session_deleted_then_removes_the_record() {
    // Causes: the fixtures below establish `delete broadcasts session deleted then removes the
    // record` with the concrete inputs, state, dependencies, and failure triggers used by this
    // case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `delete broadcasts session deleted then removes the record` is one
    // independent branch selecting `all output, state, side-effect, error, and terminal assertions
    // below hold together`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
    let runtime = EndSessionRecorder::default();
    runtime
        .block_quiesce
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let state = Arc::new(ManagedState::new(runtime.clone()));
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

    let deletion = {
        let state = state.clone();
        let id = id.clone();
        tokio::spawn(async move { state.delete_session(&id).await })
    };
    runtime.quiesce_entered.notified().await;

    // The terminal frame reached the open stream before the record was dropped.
    let event = rx
        .try_recv()
        .expect("a frame was broadcast to the open stream");
    assert_eq!(
        event.type_str(),
        "session.deleted",
        "the broadcast terminal frame is session.deleted"
    );

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

    // Cancelling the HTTP-side waiter after the durable fence cannot abandon
    // application-owned cleanup or make the removed projection visible again.
    deletion.abort();
    runtime.quiesce_release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if runtime.ended.lock().unwrap().as_slice() == [id.as_str()] {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached cleanup survives Delete waiter cancellation");
    assert!(matches!(state.get_session(&id), Err(StateError::NotFound)));
}

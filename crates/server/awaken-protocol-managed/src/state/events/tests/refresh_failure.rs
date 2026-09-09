//! Failure-frame checks for the actual asynchronous committed-prefix refresh.

use super::*;
use crate::state::session_record::ManagedLiveOverlay;
use crate::state::test_support::ConflictInjectingRepo;

/// The frame includes the CAS token and every visible/continuation field.
/// Exhaustive destructuring makes adding a SessionRecord field require an
/// oracle decision. This deliberately does not call the publisher's content
/// comparator: that comparator excludes the CAS token and is itself subject
/// to the independent frame assertion.
pub(super) fn assert_record_frame_eq(actual: &SessionRecord, expected: &SessionRecord) {
    let SessionRecord {
        cache_revision,
        agent_id,
        session,
        resource_state,
        events,
        checkpoint,
        overlay,
        deferred_session_stop_reason,
        event_thread_owners,
        child_threads,
        primary_thread_usage,
    } = actual;
    assert_eq!(*cache_revision, expected.cache_revision, "frame: cache CAS");
    assert_eq!(agent_id, &expected.agent_id, "frame: agent identity");
    assert!(
        resource_state == &expected.resource_state,
        "frame: resources"
    );
    assert!(
        checkpoint == &expected.checkpoint,
        "frame: source/progress pair"
    );
    assert_eq!(
        deferred_session_stop_reason, &expected.deferred_session_stop_reason,
        "frame: deferred terminal continuation"
    );
    assert_eq!(
        event_thread_owners, &expected.event_thread_owners,
        "frame: event owners"
    );
    let ManagedLiveOverlay {
        pending_events,
        anchors,
    } = overlay;
    assert_eq!(anchors, &expected.overlay.anchors, "frame: overlay anchors");
    for (label, actual, expected) in [
        (
            "session",
            serde_json::to_value(session),
            serde_json::to_value(&expected.session),
        ),
        (
            "events",
            serde_json::to_value(events),
            serde_json::to_value(&expected.events),
        ),
        (
            "pending overlays",
            serde_json::to_value(pending_events),
            serde_json::to_value(&expected.overlay.pending_events),
        ),
        (
            "child threads",
            serde_json::to_value(child_threads),
            serde_json::to_value(&expected.child_threads),
        ),
        (
            "primary usage",
            serde_json::to_value(primary_thread_usage),
            serde_json::to_value(&expected.primary_thread_usage),
        ),
    ] {
        assert_eq!(actual.unwrap(), expected.unwrap(), "frame: {label}");
    }
}

#[tokio::test]
async fn exhausted_projection_cache_revision_preserves_complete_warm_frame() {
    // Cause/effect table at the last fallible publication operation:
    // C1 warm record already exists; C2 a stable committed Running/text prefix
    // builds a changed candidate; C3 the existing cache CAS is u64::MAX.
    // R1 C1+C2+C3 => the real checked increment returns its exhaustion error,
    // preserves every CURRENT frame field, and sends no subscriber event.
    // This is the numeric boundary of the actual publisher after the full
    // private reducer, not a claim that a practical workload reaches 2^64
    // publications. Earlier IO and correlation failures have separate rules.
    let runtime = LifecycleRuntime::default();
    let state = ManagedState::new(runtime.clone());
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "coder", "environment_id": "env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    state.application.begin_activity(&session.id).await.unwrap();
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        1,
        &session.id,
        &RunId("cache-exhaustion-run".into()),
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));
    runtime.replace_committed_messages(vec![(
        2,
        Message::text(
            MessageId("cache-exhaustion-message".into()),
            Role::Assistant,
            "the candidate must remain private on exhaustion",
        ),
    )]);
    let before = {
        let mut sessions = state.sessions.lock().unwrap();
        let current = sessions.get_mut(&session.id).unwrap();
        current.cache_revision = u64::MAX;
        current.clone()
    };
    let (_, mut live) = state.stream_subscribe(&session.id).unwrap();
    let error = state
        .refresh_committed_events(&session.id)
        .await
        .expect_err("R1 candidate increment must fail before publication");
    assert!(
        error
            .to_string()
            .contains("Managed projection cache revision exhausted"),
        "R1 exact final publication failure: {error}"
    );
    assert_record_frame_eq(
        state.sessions.lock().unwrap().get(&session.id).unwrap(),
        &before,
    );
    assert!(
        matches!(
            live.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "R1 failed final publication must not broadcast"
    );
}

async fn read_cut_fixture(with_child: bool) -> (RehydrateFake, Arc<ManagedState>, String) {
    let runtime = RehydrateFake::default();
    runtime
        .delegate_ids
        .lock()
        .unwrap()
        .push("researcher".into());
    let inner = Arc::new(ephemeral_session_repo());
    let repository = Arc::new(ConflictInjectingRepo::new(
        inner.clone(),
        0,
        runtime.reads.clone(),
    ));
    let state = Arc::new(ManagedState::new(runtime.clone()).with_session_repo(repository));
    let id = if with_child {
        let session = state
            .create_session(
                serde_json::from_value(serde_json::json!({
                    "agent": "coder", "environment_id": "env_local"
                }))
                .unwrap(),
                None,
            )
            .await
            .unwrap();
        let root_run = RunId("refresh-read-root".into());
        let child_run = RunId("refresh-read-child".into());
        runtime.install_agent_coordination_prefix(
            &session.id,
            root_run.clone(),
            0,
            "refresh-child-call",
            CoordinatedThreadLink {
                session_id: session.id.clone(),
                thread_id: ThreadId("refresh-child".into()),
                target: CoordinatedThreadTarget::Agent {
                    agent_id: "researcher".into(),
                },
                created_by_operation_id: ToolBatch::operation_id_for_step(
                    &root_run,
                    0,
                    "refresh-child-call",
                ),
                latest_run_id: Some(child_run.clone()),
            },
        );
        runtime.lifecycle.lock().unwrap().push(lifecycle(
            1,
            "refresh-child",
            &child_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ));
        runtime.committed_by_thread.lock().unwrap().insert(
            "refresh-child".into(),
            vec![Message::text(
                MessageId("refresh-child-message".into()),
                Role::Assistant,
                "warm child transcript",
            )],
        );
        session.id
    } else {
        let id = "refresh-retained-outcome";
        let (persisted, _) = retained_outcome_fixture(id, "refresh-outcome", 1);
        crate::state::test_support::create_session_fixture(
            inner.as_ref(),
            DEFAULT_SCOPE,
            persisted,
        )
        .await;
        id.into()
    };
    state
        .refresh_committed_events(&id)
        .await
        .expect("the fixture builds a real warm projection");
    runtime.reads.reset();
    (runtime, state, id)
}

#[tokio::test]
async fn every_refresh_read_failure_preserves_current_frame_after_competing_publication() {
    // Actual-call cause/effect table, not a copy of the reducer/selector:
    // C1 retained Outcome with no child, or one accepted coordinated child;
    // C2 every ordinal of the COMPLETE observed warm-refresh read trace below;
    // C3 no competing writer, or a real durable Session title update publishes
    // through the application's update path while C2's read is paused.
    // R1 each C1 x C2 x C3 preserves ENTRY through the paused read, returns its
    // exact injected Unavailable and runs no later refresh read; the full frame
    // at RELEASE (including a competitor's
    // legitimate success) survives and the failed refresh broadcasts nothing.
    // R2 remove the read fault => real refresh and replay succeed; replay keeps
    // the complete frame. The first repository cut precedes candidate cloning;
    // all others follow it. The separate orphan test owns private-reducer errors
    // and the exhaustion test owns the final publisher error.
    // Bounds: these two actual topologies, one lifecycle page, one retained
    // Outcome or one child, every listed read ordinal, both writer schedules.
    // This covers control-flow cuts, not arbitrary topology/SQL refinement.
    let mut checked_rules = 0;
    for with_child in [false, true] {
        let expected = if with_child {
            vec![
                "session",
                "root_snapshot",
                "coordination",
                "child_snapshot",
                "lifecycle",
                "usage",
                "coordination",
                "child_usage",
                "session",
                "coordination",
                "child_snapshot",
                "root_snapshot",
                "session",
            ]
        } else {
            vec![
                "session",
                "outcome",
                "root_snapshot",
                "coordination",
                "lifecycle",
                "usage",
                "coordination",
                "session",
                "coordination",
                "root_snapshot",
                "session",
            ]
        };
        let (runtime, state, id) = read_cut_fixture(with_child).await;
        state.refresh_committed_events(&id).await.unwrap();
        assert_eq!(
            runtime.reads.trace(),
            expected,
            "R0 exact actual read boundary"
        );
        for (cut, port) in expected.iter().enumerate().rev() {
            for competing in [false, true] {
                let (runtime, state, id) = read_cut_fixture(with_child).await;
                let entry = state.sessions.lock().unwrap().get(&id).unwrap().clone();
                let (_, mut live) = state.stream_subscribe(&id).unwrap();
                runtime.reads.fail_at(cut);
                let (entered, release) = runtime.reads.pause_at(cut);
                let refresh = {
                    let state = state.clone();
                    let id = id.clone();
                    tokio::spawn(async move { state.refresh_committed_events(&id).await })
                };
                tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
                    .await
                    .expect("R1 every configured read cut is actually reached");
                assert_eq!(
                    runtime.reads.trace(),
                    expected[..=cut],
                    "R1 exact prefix before pause"
                );
                assert_record_frame_eq(state.sessions.lock().unwrap().get(&id).unwrap(), &entry);
                if competing {
                    state
                        .update_session(
                            &id,
                            awaken_session_application::SessionUpdateCommand {
                                title: Some(
                                    awaken_session_application::SessionFieldUpdate::Replace(
                                        "competing publisher".into(),
                                    ),
                                ),
                                metadata: None,
                                budget: None,
                                tools: None,
                                mcp_update: None,
                                idempotency_key: None,
                                request_fingerprint: "refresh-competing-title".into(),
                                if_match: None,
                            },
                        )
                        .await
                        .expect("R1 competing durable update succeeds independently");
                    let current = state.sessions.lock().unwrap().get(&id).unwrap().clone();
                    assert!(
                        current.cache_revision > entry.cache_revision,
                        "R1 actual competing publisher advances cache"
                    );
                    assert_eq!(
                        current.session.title.as_deref(),
                        Some("competing publisher")
                    );
                    assert!(
                        matches!(
                            live.try_recv().unwrap().kind,
                            OutboundKind::SessionUpdated { .. }
                        ),
                        "R1 successful competing publication is visible before the failure"
                    );
                }
                let before_failure = state.sessions.lock().unwrap().get(&id).unwrap().clone();
                let reads_before_release = runtime.reads.trace();
                release.notify_one();
                let error = tokio::time::timeout(std::time::Duration::from_secs(5), refresh)
                    .await
                    .unwrap()
                    .unwrap()
                    .expect_err("R1 selected actual read must fail closed");
                assert!(
                    error
                        .to_string()
                        .contains(&format!("scripted {port} read failure")),
                    "R1 exact error at topology={with_child} cut={cut} competing={competing}: {error}"
                );
                assert_eq!(
                    runtime.reads.trace(),
                    reads_before_release,
                    "R1 error terminates this refresh without later reads"
                );
                assert_record_frame_eq(
                    state.sessions.lock().unwrap().get(&id).unwrap(),
                    &before_failure,
                );
                assert!(
                    matches!(
                        live.try_recv(),
                        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                    ),
                    "R1 failure must not broadcast after another writer's legitimate event"
                );
                runtime.reads.reset();
                state
                    .refresh_committed_events(&id)
                    .await
                    .expect("R2 fault removal recovers without restart");
                let repaired = state.sessions.lock().unwrap().get(&id).unwrap().clone();
                state
                    .refresh_committed_events(&id)
                    .await
                    .expect("R2 repaired prefix replays");
                assert_record_frame_eq(state.sessions.lock().unwrap().get(&id).unwrap(), &repaired);
                checked_rules += 1;
            }
        }
    }
    assert_eq!(
        checked_rules, 48,
        "R1-R2 every read cut and competing-writer schedule executes"
    );
}

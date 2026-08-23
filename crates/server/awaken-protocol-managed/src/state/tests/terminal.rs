use super::*;
use awaken_runtime_contract::tool_batch::ToolBatch;
use awaken_session_contract::ToolPermissionDecision;

fn test_thread_agent(id: &str) -> SessionThreadAgent {
    ManagedState::thread_agent_from_profile(id, Default::default())
}

async fn await_session_tombstone(repo: &dyn ManagedSessionRepository, id: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if matches!(
                repo.get(id).await,
                Err(awaken_session_contract::SessionRepositoryError::NotFound)
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached terminal cleanup converges to a tombstone");
}

#[tokio::test]
async fn coordinator_only_creation_reports_durable_preparing_without_fabricated_worker_ack() {
    // Causes: the fixtures below establish `coordinator only creation reports durable preparing
    // without fabricated worker ack` with the concrete inputs, state, dependencies, and failure
    // triggers used by this case.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 placement is a registered Worker; C2 the durable
    // creation intent and WorkQueue dispatch succeed; C3 that queue has no
    // physical-realization acknowledgement protocol. Effects: E1 create returns
    // the truthful Preparing projection; E2 the durable aggregate records Worker
    // placement; E3 no initial-idle lifecycle fact is fabricated; E4 Control only
    // prepares the Thread identity. Decision table: R1 C1+C2+C3 => E1+E2+E3+E4.
    // FMECA: treating enqueue as readiness could admit a Run before realization
    // (severity 9, occurrence 5, detection 7), while waiting for an acknowledgement
    // that cannot arrive times out every healthy split-process create (severity 8,
    // occurrence 10, detection 3). The durable Preparing projection plus strict
    // Run admission is the single protocol-supported boundary.
    let runtime = EndSessionRecorder::default();
    let prepared = runtime.prepared.clone();
    let runtime = Arc::new(runtime);
    let repo = Arc::new(ephemeral_session_repo());
    let environments = crate::test_support::environment_components().1;
    let application = awaken_session_application::SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(mcp_attachment::UnsupportedMcpAttachmentRealizer),
        repo.clone(),
        environments.clone(),
        awaken_session_application::SessionApplicationConfiguration {
            execution_placement:
                awaken_session_application::SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let state = ManagedState::from_application(Arc::new(application), environments);
    let created = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "assistant", "environment_id": "env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .expect("R1/E1 durable demand is accepted");
    assert_eq!(created.status, SessionStatus::Rescheduling, "R1/E1");
    let scan = repo.reconcilable_sessions().await.unwrap();
    assert_eq!(scan.sessions.len(), 1, "R1/E2");
    let persisted = &scan.sessions[0];

    assert!(
        persisted
            .session
            .frozen_baseline()
            .is_some_and(|baseline| baseline.runtime_placement
                == awaken_session_contract::SessionRuntimePlacement::Worker),
        "P1 freezes the Runtime placement fact"
    );
    assert!(persisted.session.realization.is_none(), "P1/E2");
    assert!(
        matches!(
            persisted.session.environment,
            awaken_session_contract::SessionEnvironmentState::Unmaterialized
        ),
        "P1/E3"
    );
    assert_eq!(
        prepared.lock().unwrap().as_slice(),
        std::slice::from_ref(&persisted.session.session_id),
        "R1/E4 only prepares the thread identity"
    );
    assert!(repo.pending_lifecycle().await.unwrap().is_empty(), "R1/E3");
}

/// Cause/effect graph: C1 the child is a real coordinated Thread link; C2 its
/// latest ordinary Run is Idle; C3 it is still Running; C4 the durable archive
/// command commits Archived through the parent Session partition; C5 the same
/// request retries; C6 the disposable Managed cache is lost; C7 the id is
/// unknown; C8 the Runtime fixture exposes one already-committed coordinated
/// prefix before Thread assertions. Effects: E1 C1+C2+C4+C8 returns Terminated and emits one terminal; E2
/// C3 conflicts before the command; E3 C5 is idempotent; E4 C6 rebuilds the same
/// terminal event id from disposition truth; E5 C7 is not found. No child-named
/// teardown or Managed archive registry participates.
///
/// | Rule | Real | State | Archived | Retry | Cold | Result/effect |
/// |---|---|---|---|---|---|---|
/// | A1 | yes | idle | no→yes | no | no | E1 |
/// | A2 | yes | running | no | no | no | E2 |
/// | A3 | yes | terminated | yes | yes | no | E3 |
/// | A4 | yes | n/a | yes | no | yes | E4 |
/// | A5 | no | n/a | n/a | no | no | E5 |
#[tokio::test]
async fn child_thread_archive_uses_durable_disposition_and_survives_cache_loss() {
    // Causes: the fixtures below establish `child thread archive` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let runtime = crate::test_support::CoordinatedRuntimeFake::default();
    let repo = Arc::new(ephemeral_session_repo());
    let state = Arc::new(ManagedState::new(runtime.clone()).with_session_repo(repo.clone()));
    let request = serde_json::from_value(serde_json::json!({
        "agent": "coder", "environment_id": "env_local"
    }))
    .unwrap();
    let session = state.create_session(request, None).await.unwrap();
    <crate::test_support::CoordinatedRuntimeFake as SessionRuntime>::run(
        &runtime,
        "coder",
        &session.id,
        vec![ContentBlock::text("coordinate")],
    )
    .await
    .expect("A1/C8 installs one committed coordinated recovery prefix");
    state.refresh_committed_events(&session.id).await.unwrap();
    let child_id = crate::test_support::CoordinatedRuntimeFake::CHILD_THREAD_ID;
    assert_eq!(
        state.get_thread(&session.id, child_id).unwrap().status,
        SessionThreadStatus::Idle,
        "A1"
    );

    {
        let mut sessions = state.sessions.lock().unwrap();
        let child = sessions
            .get_mut(&session.id)
            .unwrap()
            .child_threads
            .iter_mut()
            .find(|thread| thread.id == child_id)
            .unwrap();
        child.status = SessionThreadStatus::Running;
    }
    assert!(
        matches!(
            state.archive_thread(&session.id, child_id).await,
            Err(StateError::Conflict)
        ),
        "A2/E2"
    );
    assert!(runtime.archive_commits().is_empty(), "A2/E2");
    state
        .sessions
        .lock()
        .unwrap()
        .get_mut(&session.id)
        .unwrap()
        .child_threads
        .iter_mut()
        .find(|thread| thread.id == child_id)
        .unwrap()
        .status = SessionThreadStatus::Idle;

    let archived = state
        .archive_thread(&session.id, child_id)
        .await
        .expect("A1/E1 commits the durable disposition");
    assert_eq!(archived.status, SessionThreadStatus::Terminated, "A1/E1");
    assert_eq!(
        runtime.archive_commits(),
        vec![(session.id.clone(), child_id.into())],
        "A1/E1 uses the parent+logical-child identity"
    );
    let warm_terminal_events = state
        .list_thread_events(&session.id, child_id, None, None)
        .unwrap()
        .data
        .into_iter()
        .filter(|event| event.type_str() == "session.thread_status_terminated")
        .collect::<Vec<_>>();
    assert_eq!(warm_terminal_events.len(), 1, "A1/E1");
    let warm_terminal_id = warm_terminal_events[0].id.clone();

    let retried = state.archive_thread(&session.id, child_id).await.unwrap();
    assert_eq!(retried.status, SessionThreadStatus::Terminated, "A3/E3");
    assert_eq!(runtime.archive_commits().len(), 1, "A3/E3");
    assert_eq!(
        state
            .list_thread_events(&session.id, child_id, None, None)
            .unwrap()
            .data
            .iter()
            .filter(|event| event.type_str() == "session.thread_status_terminated")
            .count(),
        1,
        "A3/E3"
    );
    assert!(
        matches!(
            state.archive_thread(&session.id, "sthr_unknown").await,
            Err(StateError::NotFound)
        ),
        "A5/E5"
    );

    let restarted = ManagedState::new(runtime.clone()).with_session_repo(repo);
    restarted.ensure_session(&session.id).await.unwrap();
    assert_eq!(
        restarted.get_thread(&session.id, child_id).unwrap().status,
        SessionThreadStatus::Terminated,
        "A4/E4"
    );
    let cold_terminal_events = restarted
        .list_thread_events(&session.id, child_id, None, None)
        .unwrap()
        .data
        .into_iter()
        .filter(|event| event.type_str() == "session.thread_status_terminated")
        .collect::<Vec<_>>();
    assert_eq!(cold_terminal_events.len(), 1, "A4/E4");
    assert_eq!(cold_terminal_events[0].id, warm_terminal_id, "A4/E4 id");
}

#[tokio::test]
async fn child_thread_archive_failure_commits_no_terminal_projection() {
    // Causes: the fixtures below establish `child thread archive failure commits no terminal
    // projection` with the concrete inputs, state, dependencies, and failure triggers used by this
    // case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 a real child is Idle; C2 the parent-partition
    // disposition commit fails; C3 the Runtime fixture exposes one committed
    // coordinated prefix. Effect E1 the error is returned while both the
    // disposable Thread and event stream remain unchanged. Decision rule F1 is
    // C1+C2=>E1; command-success/retry/restart rules are covered by the archive
    // decision table above.
    let runtime = crate::test_support::CoordinatedRuntimeFake::default();
    let state = Arc::new(ManagedState::new(runtime.clone()));
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
    <crate::test_support::CoordinatedRuntimeFake as SessionRuntime>::run(
        &runtime,
        "coder",
        &session.id,
        vec![ContentBlock::text("coordinate")],
    )
    .await
    .expect("F1/C3 installs one committed coordinated recovery prefix");
    state.refresh_committed_events(&session.id).await.unwrap();
    let child_id = crate::test_support::CoordinatedRuntimeFake::CHILD_THREAD_ID;
    let before = state
        .list_thread_events(&session.id, child_id, None, None)
        .unwrap()
        .data
        .len();
    runtime.reject_archive(true);

    let error = state
        .archive_thread(&session.id, child_id)
        .await
        .expect_err("F1 disposition failure must propagate");
    assert!(
        error
            .to_string()
            .contains("archive disposition commit failed")
    );
    assert_eq!(
        state.get_thread(&session.id, child_id).unwrap().status,
        SessionThreadStatus::Idle,
        "F1/E1"
    );
    assert_eq!(
        state
            .list_thread_events(&session.id, child_id, None, None)
            .unwrap()
            .data
            .len(),
        before,
        "F1/E1"
    );
    assert!(runtime.archive_commits().is_empty(), "F1/E1");
}

/// Cause/effect graph: optional `session_thread_id` -> canonical runtime
/// Thread selection -> interrupt side effects. A named live Thread selects
/// exactly itself; an absent selector fans out to the primary and every
/// non-terminal child; an unknown selector or the retired primary sentinel
/// fails admission before the
/// receipt/event log or runtime changes. The durable archived-child branch is
/// covered with the archive disposition port rather than a fabricated cache row.
/// The Runtime fixture first exposes one committed coordinated prefix, so the
/// selector rules remain isolated from User Event admission timing.
///
/// Decision table:
/// | rule | selector | target state | runtime keys | persisted receipt |
/// |---|---|---|---|---|
/// | I1 | child id | idle/requires-action | child only | yes |
/// | I2 | primary id | live | Session id only | yes |
/// | I3 | absent | root + live child | parent partition root + child | yes |
/// | I4 | unknown/retired id | absent | none | no |
/// | I5 | absent | one target fails | every target attempted, durable receipt pending | yes |
/// | I6 | I5 retry | all targets healthy | same frozen set, processed once | same receipt |
#[tokio::test]
async fn interrupt_selector_targets_one_thread_or_all_non_terminal_threads() {
    // Causes: the fixtures below establish `interrupt selector targets one thread or all non
    // terminal threads` with the concrete inputs, state, dependencies, and failure triggers used by
    // this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let runtime = crate::test_support::CoordinatedRuntimeFake::default();
    let state = Arc::new(ManagedState::new(runtime.clone()));
    let request = serde_json::from_value(serde_json::json!({
        "agent": "coder", "environment_id": "env_local"
    }))
    .unwrap();
    let session = state.create_session(request, None).await.unwrap();
    <crate::test_support::CoordinatedRuntimeFake as SessionRuntime>::run(
        &runtime,
        "coder",
        &session.id,
        vec![ContentBlock::text("coordinate")],
    )
    .await
    .expect("I1-I5 setup installs one committed coordinated recovery prefix");
    state.refresh_committed_events(&session.id).await.unwrap();
    let child_id = crate::test_support::CoordinatedRuntimeFake::CHILD_THREAD_ID;
    let primary_id = state
        .list_threads(&session.id)
        .unwrap()
        .into_iter()
        .find(|thread| thread.parent_thread_id.is_none())
        .map(|thread| thread.id)
        .expect("public primary Thread");
    assert!(primary_id.starts_with("sthr_"), "I2 public codec");

    let send = |event| SendEventsRequest {
        events: vec![event],
    };
    state
        .send_events(
            &session.id,
            send(InboundEvent::UserInterrupt {
                session_thread_id: Some(child_id.into()),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        runtime.take_interrupts(),
        vec![format!("child:{}:{child_id}", session.id)],
        "I1"
    );
    assert_eq!(
        state
            .list_thread_events(&session.id, child_id, None, None)
            .unwrap()
            .data
            .iter()
            .filter(|event| event.type_str() == "user.interrupt")
            .count(),
        1,
        "I1 is visible on the selected child Thread stream"
    );

    state
        .send_events(
            &session.id,
            send(InboundEvent::UserInterrupt {
                session_thread_id: Some(primary_id),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        runtime.take_interrupts(),
        vec![format!("primary:{}", session.id)],
        "I2"
    );

    state
        .send_events(
            &session.id,
            send(InboundEvent::UserInterrupt {
                session_thread_id: None,
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        runtime.take_interrupts(),
        vec![
            format!("primary:{}", session.id),
            format!("child:{}:{child_id}", session.id),
        ],
        "I3 uses the parent partition for the logical child"
    );
    assert_eq!(
        state
            .list_thread_events(&session.id, child_id, None, None)
            .unwrap()
            .data
            .iter()
            .filter(|event| event.type_str() == "user.interrupt")
            .count(),
        2,
        "I3's selector-free interrupt is visible on every live child stream"
    );

    let event_count = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data
        .len();
    for invalid_thread_id in ["sthr_unknown".into(), format!("{}:primary", session.id)] {
        let error = state
            .send_events(
                &session.id,
                send(InboundEvent::UserInterrupt {
                    session_thread_id: Some(invalid_thread_id.clone()),
                }),
            )
            .await
            .expect_err("I4 rejects an unknown or retired selector");
        assert!(matches!(error, StateError::Run(_)), "I4: {error}");
        assert!(runtime.take_interrupts().is_empty(), "I4");
        assert_eq!(
            state
                .list_events(&session.id, None, None, false)
                .unwrap()
                .data
                .len(),
            event_count,
            "I4 admission is atomic for {invalid_thread_id}"
        );
    }

    runtime.reject_interrupt(Some(format!("primary:{}", session.id)));
    let receipt = state
        .send_events(
            &session.id,
            send(InboundEvent::UserInterrupt {
                session_thread_id: None,
            }),
        )
        .await
        .expect("I5 durably acknowledges the complete command before its effect");
    assert_eq!(receipt.data.len(), 1, "I5 one retained receipt");
    assert!(receipt.data[0].processed_at.is_none(), "I5 remains pending");
    assert_eq!(
        runtime.take_interrupts(),
        vec![
            format!("primary:{}", session.id),
            format!("child:{}:{child_id}", session.id),
        ],
        "I5 attempts every frozen non-terminal target despite an earlier failure"
    );
    runtime.reject_interrupt(None);
    state
        .application
        .drive_session_event_batches(&session.id, None)
        .await
        .expect("I6 cold-safe retry completes the same retained command");
    assert_eq!(
        runtime.take_interrupts(),
        vec![
            format!("primary:{}", session.id),
            format!("child:{}:{child_id}", session.id),
        ],
        "I6 reuses the exact frozen target set"
    );
    state.refresh_committed_events(&session.id).await.unwrap();
    let persisted = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data
        .into_iter()
        .find(|event| event.id == receipt.data[0].id)
        .expect("I6 retained interrupt receipt");
    assert!(
        persisted.processed_at.is_some(),
        "I6 marks it processed once"
    );
    state
        .application
        .drive_session_event_batches(&session.id, None)
        .await
        .expect("I6 processed replay is a no-op");
    assert!(
        runtime.take_interrupts().is_empty(),
        "I6 no duplicate effect"
    );
}

// Delete finalization tests are generated from this causal graph:
//
// C1 terminal CAS committed ──> E1 public reads are NotFound
//                         └───> C2 external cleanup attempted
// C2 cleanup succeeds ────────> E2 durable row becomes a tombstone
// C2 cleanup fails ───────────> E3 hidden cleanup row remains pending
// E3 + C3 later retry succeeds -> E2
//
// Decision table ("cleanup" includes sandbox and Repository cleanup):
//
// | Rule | C1 | C2 | C3 | E1 | E2 | E3 |
// |------|----|----|----|----|----|----|
// | D1   | T  | T  | -  | T  | T  | F  |
// | D2   | T  | F  | F  | T  | F  | T  |
// | D3   | T  | F  | T  | T  | T  | F  |
//
// D1, D2 and D3 respectively generate the success, failure, and restart
// recovery tests below. No test invents a second cleanup implementation.

/// D1: `DELETE /v1/sessions/{id}` reaches the host's terminal sandbox
/// disposal and converges the hidden durable row to a tombstone.
#[tokio::test]
async fn delete_session_disposes_the_host_sandbox() {
    let rt = EndSessionRecorder::default();
    let ended = rt.ended.clone();
    let repo = Arc::new(ephemeral_session_repo());
    let state = ManagedState::new(rt).with_session_repo(repo.clone());
    let id = state
        .create_session(bare_create_params(), None)
        .await
        .expect("create")
        .id;
    state.delete_session(&id).await.expect("delete");
    await_session_tombstone(repo.as_ref(), &id).await;
    assert_eq!(
        *ended.lock().unwrap(),
        vec![id.clone()],
        "delete tears down the session's sandbox via end_session"
    );
    assert!(
        matches!(
            repo.get(&id).await,
            Err(awaken_session_contract::SessionRepositoryError::NotFound)
        ),
        "successful cleanup converges to a tombstone"
    );
}

/// Archive FMECA and cause/effect graph. Failure modes are FM1 terminal fact
/// commits without Runtime cleanup, FM2 replay disposes the same environment
/// twice, and FM3 cleanup runs before the durable terminal fence. Causes: C1
/// live Session, C2 first archive, C3 archived Session, C4 replay. Effects: E1
/// terminal state/fact then one cleanup, E2 identical terminal state and no
/// second cleanup. Cause graph: C1&&C2 -> E1; C3&&C4 -> E2.
///
/// | Rule | State | Command | Durable transition | Cleanup | Effect |
/// |---|---|---|---|---|---|
/// | A1 | live | archive | once | once, after fence | E1 |
/// | A2 | archived | archive | none | none | E2 |
#[tokio::test]
async fn archive_session_disposes_on_the_terminal_transition_only() {
    let rt = EndSessionRecorder::default();
    let ended = rt.ended.clone();
    let state = ManagedState::new(rt);
    let id = state
        .create_session(bare_create_params(), None)
        .await
        .expect("create")
        .id;
    state.archive_session(&id).await.expect("archive");
    assert_eq!(
        *ended.lock().unwrap(),
        vec![id.clone()],
        "archive reaps the sandbox on the terminal transition"
    );
    state.archive_session(&id).await.expect("re-archive");
    assert_eq!(
        *ended.lock().unwrap(),
        vec![id],
        "a re-archive (idempotent) does not re-dispose"
    );
}

/// Parent-terminal projection cause/effect graph. C1 a durable coordinated
/// child is Running, Awaiting, or Idle; C2 the parent archive commits its one
/// absorbing PersistedSession terminal fact; C3 a live subscriber is already
/// attached; C4 the disposable Managed cache is lost. Effects: E1 every
/// nonterminated child gets exactly one terminal after its visible history; E2
/// that child terminal is broadcast before the aggregate terminal; E3 primary
/// closes only on the aggregate terminal; E4 cold recovery derives the same
/// terminal Thread DTO/event ids without a child disposition write; E5 the
/// primary emits its own terminated edge, using the one public `sthr_` id,
/// between child termination and aggregate Session termination.
///
/// | Rule | Child before archive | Parent terminal | Live | Cold | Effects |
/// |---|---|---|---|---|---|
/// | P1 | Running | yes | yes | yes | E1,E2,E3,E4,E5 |
/// | P2 | Awaiting | yes | yes | yes | E1,E2,E3,E4,E5 |
/// | P3 | Idle | yes | yes | yes | E1,E2,E3,E4,E5 |
#[tokio::test]
async fn parent_terminal_closes_every_derived_child_live_and_after_restart() {
    // Causes: the fixtures below establish `parent terminal` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
    use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent, RunLifecycleEventKind};
    use awaken_session_contract::{CoordinatedThreadLink, CoordinatedThreadTarget, Pending};

    for (rule, terminal_kind, terminal_state, expects_idle) in [
        ("P1", None, RunState::Running, SessionThreadStatus::Running),
        (
            "P2",
            Some(RunLifecycleEventKind::Awaiting),
            RunState::Awaiting,
            SessionThreadStatus::Idle,
        ),
        (
            "P3",
            Some(RunLifecycleEventKind::Completed),
            RunState::Ended(EndCause::NaturalEnd),
            SessionThreadStatus::Idle,
        ),
    ] {
        let repo = Arc::new(ephemeral_session_repo());
        let runtime = RehydrateFake::default();
        runtime
            .delegate_ids
            .lock()
            .unwrap()
            .push("researcher".into());
        let state = Arc::new(ManagedState::new(runtime.clone()).with_session_repo(repo.clone()));
        let session = state
            .create_session(
                serde_json::from_value(serde_json::json!({
                    "agent":"coder", "environment_id":"env_local"
                }))
                .unwrap(),
                None,
            )
            .await
            .expect(rule);
        let child_id = format!("thread-parent-terminal-{rule}");
        let child_run = RunId(format!("run-parent-terminal-{rule}"));
        runtime
            .coordinated
            .lock()
            .unwrap()
            .push(CoordinatedThreadLink {
                session_id: session.id.clone(),
                thread_id: awaken_agent_contract::agent::thread::Id(child_id.clone()),
                target: CoordinatedThreadTarget::Agent {
                    agent_id: "researcher".into(),
                },
                created_by_operation_id: ToolBatch::operation_id_for_step(
                    &RunId("root".into()),
                    0,
                    &format!("terminal-{rule}"),
                ),
                latest_run_id: Some(child_run.clone()),
            });
        let call_id = format!("client-call-{rule}");
        let child_message = if terminal_kind == Some(RunLifecycleEventKind::Awaiting) {
            runtime.pending_by_thread.lock().unwrap().insert(
                child_id.clone(),
                Pending {
                    tool_use_id: call_id.clone(),
                    name: "client_lookup".into(),
                    input: serde_json::json!({"rule":rule}),
                    client_executed: true,
                },
            );
            Message::new(
                MessageId(format!("message-{rule}")),
                Role::Assistant,
                vec![ContentBlock::tool_use(
                    call_id,
                    "client_lookup",
                    serde_json::json!({"rule":rule}),
                )],
            )
        } else {
            Message::text(
                MessageId(format!("message-{rule}")),
                Role::Assistant,
                format!("child output {rule}"),
            )
        };
        runtime
            .committed_by_thread
            .lock()
            .unwrap()
            .insert(child_id.clone(), vec![child_message]);
        let child_thread = awaken_agent_contract::agent::thread::Id(child_id.clone());
        runtime.lifecycle.lock().unwrap().push(RunLifecycleEvent {
            cursor: RunLifecycleCursor(1),
            source_commit_cursor: 1,
            thread_id: child_thread.clone(),
            run_id: child_run.clone(),
            kind: RunLifecycleEventKind::Running,
            state: RunState::Running,
            await_reason: None,
        });
        if let Some(kind) = terminal_kind {
            runtime.lifecycle.lock().unwrap().push(RunLifecycleEvent {
                cursor: RunLifecycleCursor(2),
                source_commit_cursor: 2,
                thread_id: child_thread,
                run_id: child_run,
                kind,
                state: terminal_state,
                await_reason: None,
            });
        }
        state.refresh_committed_events(&session.id).await.unwrap();
        assert_eq!(
            state.get_thread(&session.id, &child_id).unwrap().status,
            expects_idle,
            "{rule} precondition"
        );

        let (_snapshot, mut receiver) = state.stream_subscribe(&session.id).unwrap();
        let archived = state.archive_session(&session.id).await.expect(rule);
        let warm_child = state.get_thread(&session.id, &child_id).unwrap();
        assert_eq!(
            warm_child.status,
            SessionThreadStatus::Terminated,
            "{rule}/E1"
        );
        assert_eq!(warm_child.archived_at, archived.archived_at, "{rule}/E1");
        let primary_id = state
            .list_threads(&session.id)
            .unwrap()
            .into_iter()
            .find(|thread| thread.parent_thread_id.is_none())
            .map(|thread| thread.id)
            .expect("E5 primary Thread");
        assert!(primary_id.starts_with("sthr_"), "{rule}/E5");
        let live = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        let child_terminal = live
            .iter()
            .position(|event| {
                matches!(
                    &event.kind,
                    OutboundKind::SessionThreadStatusTerminated {
                        session_thread_id,
                        ..
                    } if session_thread_id == &child_id
                )
            })
            .expect("E1/E2 live child terminal");
        let primary_terminal = live
            .iter()
            .position(|event| {
                matches!(
                    &event.kind,
                    OutboundKind::SessionThreadStatusTerminated {
                        session_thread_id,
                        ..
                    } if session_thread_id == &primary_id
                )
            })
            .expect("E5 live primary terminal");
        let aggregate_terminal = live
            .iter()
            .position(|event| matches!(event.kind, OutboundKind::SessionStatusTerminated { .. }))
            .expect("E2/E3 live aggregate terminal");
        assert!(
            child_terminal < primary_terminal && primary_terminal < aggregate_terminal,
            "{rule}/E2+E5"
        );
        let warm_child_terminal_id = live[child_terminal].id.clone();
        let warm_primary_terminal_id = live[primary_terminal].id.clone();
        let warm_aggregate_terminal_id = live[aggregate_terminal].id.clone();
        let child_projection = live
            .iter()
            .cloned()
            .filter_map(|event| {
                ManagedState::project_event_for_thread(&session.id, &child_id, event, None, false)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            child_projection
                .iter()
                .filter(|event| { event.type_str() == "session.thread_status_terminated" })
                .count(),
            1,
            "{rule}/E1"
        );
        assert!(
            child_projection
                .iter()
                .all(|event| event.type_str() != "session.status_terminated"),
            "{rule}/E3 child stream does not consume the aggregate terminator"
        );
        state.refresh_committed_events(&session.id).await.unwrap();
        assert_eq!(
            state
                .list_thread_events(&session.id, &child_id, None, None)
                .unwrap()
                .data
                .iter()
                .filter(|event| event.type_str() == "session.thread_status_terminated")
                .count(),
            1,
            "{rule}/E1 warm replay is idempotent"
        );
        let warm_primary = state
            .list_thread_events(&session.id, &primary_id, None, None)
            .unwrap()
            .data;
        assert_eq!(
            warm_primary
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    OutboundKind::SessionThreadStatusTerminated {
                        session_thread_id,
                        ..
                    } if session_thread_id == &primary_id
                ))
                .count(),
            1,
            "{rule}/E5 warm"
        );

        let restarted = ManagedState::new(runtime.clone()).with_session_repo(repo.clone());
        restarted.ensure_session(&session.id).await.expect(rule);
        let cold_thread = restarted.get_thread(&session.id, &child_id).unwrap();
        assert_eq!(
            cold_thread.status,
            SessionThreadStatus::Terminated,
            "{rule}/E4"
        );
        assert_eq!(cold_thread.archived_at, archived.archived_at, "{rule}/E4");
        let cold_child = restarted
            .list_thread_events(&session.id, &child_id, None, None)
            .unwrap()
            .data;
        assert_eq!(
            cold_child
                .iter()
                .filter(|event| event.type_str() == "session.thread_status_terminated")
                .count(),
            1,
            "{rule}/E4"
        );
        assert_eq!(
            cold_child
                .iter()
                .find(|event| event.type_str() == "session.thread_status_terminated")
                .expect("E4 cold child terminal")
                .id,
            warm_child_terminal_id,
            "{rule}/E4 child id"
        );
        let cold_primary = restarted
            .list_thread_events(&session.id, &primary_id, None, None)
            .unwrap()
            .data;
        let cold_primary_terminal = cold_primary
            .iter()
            .filter(|event| {
                matches!(
                    &event.kind,
                    OutboundKind::SessionThreadStatusTerminated {
                        session_thread_id,
                        ..
                    } if session_thread_id == &primary_id
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(cold_primary_terminal.len(), 1, "{rule}/E4+E5");
        assert_eq!(
            cold_primary_terminal[0].id, warm_primary_terminal_id,
            "{rule}/E4+E5 primary id"
        );
        let cold_aggregate = restarted
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .into_iter()
            .filter(|event| event.type_str() == "session.status_terminated")
            .collect::<Vec<_>>();
        assert_eq!(cold_aggregate.len(), 1, "{rule}/E4");
        assert_eq!(
            cold_aggregate[0].id, warm_aggregate_terminal_id,
            "{rule}/E4 aggregate id"
        );
        assert!(
            runtime.archive_commits.lock().unwrap().is_empty(),
            "{rule}/E4"
        );
    }
}

#[tokio::test]
async fn archived_session_can_be_deleted_after_projection_cache_loss() {
    let repo = Arc::new(ephemeral_session_repo());
    let state = ManagedState::new(EndSessionRecorder::default()).with_session_repo(repo.clone());
    let id = state
        .create_session(bare_create_params(), None)
        .await
        .expect("create")
        .id;
    state.archive_session(&id).await.expect("archive");
    assert!(matches!(
        repo.get(&id).await.unwrap().disposition,
        SessionDisposition::Archived { .. }
    ));

    let restarted =
        ManagedState::new(EndSessionRecorder::default()).with_session_repo(repo.clone());
    restarted
        .delete_session(&id)
        .await
        .expect("delete archived Session after restart");
    await_session_tombstone(repo.as_ref(), &id).await;
    assert!(
        matches!(
            repo.get(&id).await,
            Err(awaken_session_contract::SessionRepositoryError::NotFound)
        ),
        "delete converges to tombstone"
    );
    assert!(matches!(
        restarted.get_session(&id),
        Err(StateError::NotFound)
    ));
}

#[tokio::test]
async fn activation_failure_remains_deletable_after_projection_cache_loss() {
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    let mut failed = sample_persisted("sesn_failed_delete");
    failed.execution = SessionExecutionState::ActivationFailed;
    create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, failed).await;

    let restarted = ManagedState::new(EndSessionFailer).with_session_repo(repo.clone());
    restarted
        .delete_session("sesn_failed_delete")
        .await
        .expect("delete failed Session after restart");
    let pending = repo.get("sesn_failed_delete").await.unwrap();
    assert_eq!(
        pending.execution,
        SessionExecutionState::ActivationFailed,
        "retention does not rewrite the execution failure"
    );
    assert!(matches!(pending.disposition, SessionDisposition::Deleting));
    assert!(matches!(
        restarted.get_session("sesn_failed_delete"),
        Err(StateError::NotFound)
    ));
}

/// Terminal-cleanup cause/effect graph. C1 the primary Runtime exists; C2
/// zero or more child Runtime ids exist; C3 a duplicate child id is present;
/// C4 the terminal command is replayed. E1 every unique Runtime is torn down
/// once on the transition; E2 duplicates do not duplicate effects; E3 a
/// replay performs no teardown. Background recovery drives the same
/// application method, so there is no second cleanup algorithm.
///
/// | Rule | Primary | Children | Duplicate | Replay | Effect |
/// |---|---|---|---|---|---|
/// | T1 | yes | two | no | no | E1 |
/// | T2 | yes | two | yes | no | E1 + E2 |
/// | T3 | yes | any | any | yes | E3 |
#[tokio::test]
async fn archive_terminal_cleanup_tears_down_each_unique_runtime_once() {
    let runtime = EndSessionRecorder::default();
    let ended = runtime.ended.clone();
    let delegated = runtime.delegated.clone();
    let state = ManagedState::new(runtime);
    let id = state
        .create_session(bare_create_params(), None)
        .await
        .expect("create")
        .id;
    {
        let mut sessions = state.sessions.lock().unwrap();
        let record = sessions.get_mut(&id).unwrap();
        record.child_threads.push(ManagedState::child_thread(
            &record.session,
            "child-a",
            test_thread_agent("researcher"),
        ));
        record.child_threads.push(ManagedState::child_thread(
            &record.session,
            "child-b",
            test_thread_agent("reviewer"),
        ));
        record.child_threads.push(ManagedState::child_thread(
            &record.session,
            "child-a",
            test_thread_agent("duplicate-projection"),
        ));
    }
    *delegated.lock().unwrap() = vec![
        awaken_session_contract::DelegatedRun {
            run_id: awaken_agent_contract::agent::run::Id("child-a".into()),
            parent_call_id: "call-a".into(),
            agent_id: "researcher".into(),
            status: awaken_agent_contract::agent::delegation::DelegationStatus::Open,
        },
        awaken_session_contract::DelegatedRun {
            run_id: awaken_agent_contract::agent::run::Id("child-b".into()),
            parent_call_id: "call-b".into(),
            agent_id: "reviewer".into(),
            status: awaken_agent_contract::agent::delegation::DelegationStatus::Open,
        },
        awaken_session_contract::DelegatedRun {
            run_id: awaken_agent_contract::agent::run::Id("child-a".into()),
            parent_call_id: "duplicate".into(),
            agent_id: "duplicate-projection".into(),
            status: awaken_agent_contract::agent::delegation::DelegationStatus::Open,
        },
    ];

    state.archive_session(&id).await.expect("T1/T2");
    assert_eq!(
        ended
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([id.clone(), "child-a".into(), "child-b".into()]),
        "T1/T2"
    );
    assert_eq!(ended.lock().unwrap().len(), 3, "T2");
    state.archive_session(&id).await.expect("T3");
    assert_eq!(ended.lock().unwrap().len(), 3, "T3");
}

#[tokio::test]
async fn archive_session_rehydrates_after_process_restart() {
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    let original = ManagedState::new(EndSessionRecorder::default()).with_session_repo(repo.clone());
    let id = original
        .create_session(bare_create_params(), None)
        .await
        .expect("create")
        .id;
    drop(original);
    let runtime = EndSessionRecorder::default();
    let ended = runtime.ended.clone();
    let prepared = runtime.prepared.clone();
    let restarted = ManagedState::new(runtime).with_session_repo(repo.clone());
    let archived = restarted
        .archive_session(&id)
        .await
        .expect("archive durable Session after restart");
    assert_eq!(archived.status, SessionStatus::Terminated);
    assert_eq!(*ended.lock().unwrap(), vec![id.clone()]);
    assert!(prepared.lock().unwrap().is_empty());
    assert_eq!(
        repo.get(&id).await.unwrap().execution,
        SessionExecutionState::Terminated
    );
}

#[tokio::test]
async fn archive_session_does_not_require_a_retired_agent_publication_after_restart() {
    // Cause/effect graph: C1 durable Session freezes Agent revision 7; C2 the
    // disposable projection and exact Control publication are unavailable after
    // restart; C3 archive requests terminal cleanup. Effects: E1 the durable
    // baseline supplies the cleanup projection; E2 the Session terminates and
    // releases its Runtime exactly once; E3 ordinary interactive rehydration
    // remains fail-closed elsewhere. Requiring mutable Control presentation for
    // C3 leaks every already-terminal Run and hot-loops its downstream inbox.
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    let mut persisted = sample_persisted("sesn_retired_agent_cleanup");
    let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &persisted.baseline
    else {
        panic!("fixture baseline must be frozen");
    };
    persisted.baseline = awaken_session_contract::SessionBaselineState::Frozen(
        awaken_session_contract::SessionBaseline::compile(
            awaken_session_contract::SessionBaselineInputs {
                environment: baseline.environment.clone(),
                runtime_placement: baseline.runtime_placement,
                mcp_authoring: baseline.mcp_authoring.clone(),
                agent_id: baseline.agent_id.clone(),
                agent_revision: Some(7),
                model_override: baseline.model_override.clone(),
                model: baseline.model.clone(),
                runtime: baseline.runtime.clone(),
                delegate_ids: baseline.delegate_ids.clone(),
                toolsets: baseline.toolsets.clone(),
                mounts: baseline.mounts.clone(),
                env: baseline.env.clone(),
                prompts: baseline.prompts.clone(),
                transcript_prefix: baseline.transcript_prefix.clone(),
            },
        ),
    );
    create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, persisted).await;

    let runtime = EndSessionRecorder::default();
    let ended = runtime.ended.clone();
    let prepared = runtime.prepared.clone();
    let restarted = ManagedState::new(runtime).with_session_repo(repo.clone());
    let archived = restarted
        .archive_session("sesn_retired_agent_cleanup")
        .await
        .expect("terminal cleanup uses durable baseline without Control publication");
    assert_eq!(archived.status, SessionStatus::Terminated, "E1/E2");
    assert_eq!(archived.agent.version, 7, "E1 keeps the frozen revision");
    assert_eq!(
        ended.lock().unwrap().as_slice(),
        &["sesn_retired_agent_cleanup"],
        "E2"
    );
    assert!(
        prepared.lock().unwrap().is_empty(),
        "E2 never prepares a Runtime"
    );
    assert_eq!(
        repo.get("sesn_retired_agent_cleanup")
            .await
            .unwrap()
            .execution,
        SessionExecutionState::Terminated,
        "E2"
    );

    restarted
        .archive_session("sesn_retired_agent_cleanup")
        .await
        .expect("terminal replay is idempotent");
    assert_eq!(
        ended.lock().unwrap().len(),
        1,
        "E2 replay has no second cleanup"
    );
}

#[tokio::test]
async fn archive_persists_release_before_and_after_sandbox_teardown() {
    let repo = Arc::new(ephemeral_session_repo());
    let state = ManagedState::new(EndSessionRecorder::default()).with_session_repo(repo.clone());
    let request = serde_json::from_value(serde_json::json!({
        "agent": "assistant",
        "environment_id": "env_local",
        "resources": [{
            "type": "file",
            "file_id": "immutable-file",
            "mount_path": "/input.txt"
        }]
    }))
    .unwrap();
    let id = state.create_session(request, None).await.unwrap().id;
    assert_eq!(
        repo.get(&id).await.unwrap().resources.activations[0].state,
        awaken_session_contract::ActivationState::Active
    );

    state.archive_session(&id).await.unwrap();
    let durable = repo.get(&id).await.unwrap();
    assert_eq!(durable.execution, SessionExecutionState::Terminated);
    assert_eq!(
        durable.resources.activations[0].state,
        awaken_session_contract::ActivationState::Released
    );
    assert!(
        repo.reconcilable_sessions()
            .await
            .unwrap()
            .sessions
            .is_empty()
    );
}

#[tokio::test]
async fn session_create_enforces_the_500_file_boundary() {
    // Cause/effect rules for the Managed file-count limit:
    // R1: C1=file_count=500 → E1=create succeeds.
    // R2: C2=file_count=501 → E2=bad request before any Session persists.
    let request = |count: usize| {
        let resources = (0..count)
            .map(|index| {
                serde_json::json!({
                    "type": "file",
                    "file_id": format!("file_{index}"),
                    "mount_path": format!("/input-{index}.txt")
                })
            })
            .collect::<Vec<_>>();
        serde_json::from_value(serde_json::json!({
            "agent": "assistant",
            "environment_id": "env_local",
            "resources": resources
        }))
        .unwrap()
    };
    let state = ManagedState::new(EndSessionRecorder::default());
    assert!(state.create_session(request(500), None).await.is_ok());
    let error = state.create_session(request(501), None).await.unwrap_err();
    assert!(error.to_string().contains("at most 500 files"), "{error}");
}

/// A runtime whose sandbox teardown always fails — to prove the terminal edges
/// are BEST-EFFORT: a dispose failure is logged, never propagated, so it cannot
/// resurrect a deleted session.
struct EndSessionFailer;

#[async_trait]
impl SessionRuntime for EndSessionFailer {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }
    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }
    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }
    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<OutcomeDrive, RunError> {
        unreachable!()
    }
    async fn execute_terminal_cleanup(
        &self,
        _command: awaken_session_contract::SessionCleanupCommand,
    ) -> Result<awaken_session_contract::SessionCleanupCompletion, RunError> {
        Err(RunError::internal("sandbox dispose blew up"))
    }
    fn model(&self) -> String {
        "host-default-model".to_string()
    }
}

/// D2: a sandbox teardown failure at delete is swallowed (best-effort): the delete is
/// terminal, so the session is still removed and reads 404 afterwards — a dispose
/// error must never leave a "deleted" session alive.
#[tokio::test]
async fn delete_is_best_effort_when_sandbox_teardown_fails() {
    let repo = Arc::new(ephemeral_session_repo());
    let state = ManagedState::new(EndSessionFailer).with_session_repo(repo.clone());
    let request = serde_json::from_value(serde_json::json!({
        "agent": "assistant",
        "environment_id": "env_local",
        "resources": [{
            "type": "file",
            "file_id": "immutable-file",
            "mount_path": "/input.txt"
        }]
    }))
    .unwrap();
    let id = state
        .create_session(request, None)
        .await
        .expect("create")
        .id;
    state
        .delete_session(&id)
        .await
        .expect("delete stays terminal despite a sandbox teardown failure");
    assert!(
        matches!(state.get_session(&id), Err(StateError::NotFound)),
        "the session is gone even though its sandbox dispose errored"
    );
    let durable = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let durable = repo.get(&id).await.unwrap();
            if durable.resources.activations[0].state
                == awaken_session_contract::ActivationState::Releasing
            {
                break durable;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed detached cleanup remains durably reconcilable");
    assert!(matches!(durable.disposition, SessionDisposition::Deleting));
    assert_eq!(
        durable.resources.activations[0].state,
        awaken_session_contract::ActivationState::Releasing,
        "cleanup failure stays durable for ResourceReclaimer"
    );
    assert_eq!(
        repo.reconcilable_sessions().await.unwrap().sessions,
        vec![awaken_session_contract::ScopedPersistedSession {
            workspace_id: DEFAULT_SCOPE.to_string(),
            session: durable,
        }]
    );
}

pub(in crate::state) fn sample_persisted(id: &str) -> PersistedSession {
    let mut metadata = BTreeMap::new();
    metadata.insert("team".to_string(), "research".to_string());
    let holder = awaken_credential_contract::PlaintextHolder::new(
        awaken_credential_contract::PlaintextBoundary::Workload,
        "awaken.workload.acp",
    );
    let environment = awaken_session_contract::EnvironmentSnapshot {
        environment_id: "env_local".into(),
        revision: awaken_environment_contract::EnvironmentRevision(1),
        self_hosted: false,
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint("env-1".into()),
        sandbox: awaken_provisioning_contract::SandboxOverride {
            isolation: Some(awaken_provisioning_contract::IsolationClass::Namespace),
            ..Default::default()
        },
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_contract::SessionNetworkPolicy::None,
        credential_realization: awaken_credential_contract::CredentialRealizationProfile {
            inference_holder: holder.clone(),
            mcp_holder: holder.clone(),
            resource_holder: holder,
        },
    };
    let mut mcp = awaken_session_contract::SessionMcpAttachmentSet::from_initial(
        vec![awaken_session_contract::McpAttachmentDraft {
            name: "calc".into(),
            target: awaken_session_contract::McpTarget::parse_http("https://x").unwrap(),
            credential: None,
            prompts_as_skills: false,
            origin: awaken_session_contract::McpAttachmentOrigin::Session,
        }],
        None,
    )
    .unwrap();
    mcp.attachments[0].state = awaken_session_contract::McpAttachmentState::Active;
    PersistedSession {
        session_id: id.to_string(),
        revision: Default::default(),
        baseline: awaken_session_contract::SessionBaselineState::Frozen(
            awaken_session_contract::SessionBaseline::compile(
                awaken_session_contract::SessionBaselineInputs {
                    environment,
                    runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
                    mcp_authoring: Default::default(),
                    agent_id: "coder".into(),
                    agent_revision: None,
                    model_override: None,
                    model: "kimi-k2".into(),
                    runtime: Some("acp:custom".into()),
                    delegate_ids: Vec::new(),
                    toolsets: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                    transcript_prefix: None,
                },
            ),
        ),
        title: Some("My session".to_string()),
        metadata,
        tools: Default::default(),
        event_batches: Vec::new(),
        activity_epoch: 0,
        active_activity_epochs: Default::default(),
        running_interval: None,
        runtime_active_millis: 0,
        budget: Default::default(),
        environment: Default::default(),
        mcp,
        resources: awaken_session_contract::SessionResourceState::from_active(sample_inputs()),
        realization: None,
        realization_progress: Default::default(),
        execution: SessionExecutionState::Idle,
        disposition: Default::default(),
        terminal_cleanup: Default::default(),
    }
}

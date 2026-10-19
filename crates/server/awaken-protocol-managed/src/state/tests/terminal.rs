use super::*;
pub(in crate::state) use awaken_protocol_managed_testkit::sample_persisted;
use awaken_session_repository_contract::{LifecycleOutbox as _, SessionRepositoryReads};
use awaken_session_execution_contract::ToolPermissionDecision;
use awaken_session_types::SessionId;
use awaken_tool_contract::tool_batch::ToolBatch;

fn test_thread_agent(id: &str) -> SessionThreadAgent {
    SessionThreadAgent::from(
        &super::super::sessions::session_agent_from_configuration(
            &awaken_session_application::agent_configuration_from_profile(id, Default::default()),
        )
        .expect("valid profile fixture"),
    )
}

async fn await_session_tombstone(
    repo: &dyn awaken_session_repository_contract::SessionStore,
    id: &str,
) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if matches!(
                repo.get(&SessionId::from(id)).await,
                Err(awaken_session_state_contract::SessionRepositoryError::NotFound)
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
async fn cloud_create_returns_durable_root_before_sandbox_realization() {
    // Causes: C1 cloud Session has no initial events or one user message;
    // C2 Runtime projection has not acknowledged Sandbox readiness; C3 an
    // exact Idempotency-Key replay follows. Effects: E1 the complete root and
    // event/activity receipt commit before the HTTP-facing result; E2 the
    // logical wire status is idle/running, while durable execution remains
    // Preparing and no Runtime projection has run; E3 GET remains readable;
    // E4 replay returns that one Session without another effect. Rules R1/R2
    // per initial-event partition, R3 for GET and R4 for replay. The
    // application owner tests cold recovery separately.
    for initial_message in [false, true] {
        let (environment_authoring, environment_execution) =
            crate::state::test_support::environment_components();
        let environment_id = environment_authoring
            .application()
            .create(awaken_environment_contract::CreateEnvironmentCommand {
                command_id: format!("test:cloud-accept:{initial_message}"),
                name: "cloud accept".into(),
                description: None,
                metadata: Default::default(),
                scope: None,
                config: awaken_environment_contract::EnvironmentConfig::Cloud {
                    networking: awaken_environment_contract::EnvironmentNetworking::Unrestricted {},
                    packages: Default::default(),
                },
                sandbox_policy: None,
            })
            .await
            .expect("create exact cloud Environment")
            .id;
        let runtime = EndSessionRecorder::default();
        let prepared = runtime.prepared.clone();
        let block_prepare = runtime.block_prepare.clone();
        block_prepare.store(true, std::sync::atomic::Ordering::SeqCst);
        let state = ManagedState::new(runtime)
            .await
            .with_environments(environment_execution);
        let request = || {
            serde_json::from_value(serde_json::json!({
                "agent": "assistant",
                "environment_id": environment_id,
                "initial_events": if initial_message {
                    serde_json::json!([{"type":"user.message","content":[{"type":"text","text":"hello"}]}])
                } else {
                    serde_json::json!([])
                }
            }))
            .expect("valid Managed create")
        };
        let key = format!("cloud-accept-{initial_message}");
        let created = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            state.create_session_idempotent(request(), awaken_tenancy::DEFAULT_WORKSPACE_ID, &key),
        )
        .await
        .expect("R1/R2 create cannot wait for Sandbox readiness")
        .expect("R1/R2 durable root accepted");
        assert_eq!(
            created.status,
            if initial_message {
                SessionStatus::Running
            } else {
                SessionStatus::Idle
            },
            "R1/R2 logical status"
        );
        let root = state
            .roles
            .roots
            .session(&SessionId::from(&created.id))
            .await
            .unwrap();
        assert_eq!(
            root.execution,
            SessionExecutionState::Preparing,
            "R1/R2 durable phase"
        );
        assert_eq!(
            root.event_batches.len(),
            usize::from(initial_message),
            "R1/R2 batch"
        );
        assert_eq!(
            root.active_activity_epochs.len(),
            usize::from(initial_message),
            "R1/R2 wake"
        );
        assert!(
            prepared.lock().unwrap().is_empty(),
            "R1/R2 no physical projection"
        );
        let observed = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            state.refresh_committed_events_for_observation(&created.id, None),
        )
        .await
        .expect("R1/R2 retrieve cannot wait for Sandbox readiness");
        if let Err(error) = observed {
            assert!(
                error.is_projection_recovery_required(),
                "R1/R2 durable root remains readable"
            );
        }
        assert_eq!(
            state.get_session(&created.id).unwrap().status,
            created.status,
            "R1/R2 GET retains the root-owned logical status"
        );
        let replayed = state
            .create_session_idempotent(request(), awaken_tenancy::DEFAULT_WORKSPACE_ID, &key)
            .await
            .expect("R3 exact replay");
        assert_eq!(replayed.id, created.id, "R3 one identity");
        assert_eq!(replayed.status, created.status, "R3 one logical status");
        assert!(
            prepared.lock().unwrap().is_empty(),
            "R3 no new Runtime effect"
        );
        block_prepare.store(false, std::sync::atomic::Ordering::SeqCst);
    }
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
    let repo = Arc::new(ephemeral_session_repo().await);
    let environments = crate::state::test_support::environment_components().1;
    let application = awaken_session_application::SessionApplicationBuilder::fixture(
        runtime.clone(),
        Arc::new(awaken_protocol_managed_testkit::UnsupportedMcpAttachmentRealizer),
        awaken_session_application::SessionPersistence::new(
            repo.clone(),
            awaken_time::system_clock(),
        ),
        environments.clone(),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .configuration(
        awaken_session_application::SessionApplicationConfiguration {
            execution_placement:
                awaken_session_application::SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    )
    .build()
    .expect("valid fixture Session application");
    let state = ManagedState::from_application(
        Arc::new(application),
        environments,
        awaken_data_governance_contract::CapturePolicy::default(),
    );
    let created = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "assistant", "environment_id": "env_local"
            }))
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .expect("R1/E1 durable demand is accepted");
    assert_eq!(created.status, SessionStatus::Rescheduling, "R1/E1");
    let scan = repo.reconciliation_scan(None).await.unwrap();
    assert_eq!(scan.sessions.len(), 1, "R1/E2");
    let persisted = &scan.sessions[0];

    assert!(
        persisted
            .session
            .frozen_baseline()
            .is_some_and(|baseline| baseline.runtime_placement
                == awaken_session_types::SessionRuntimePlacement::Worker),
        "P1 freezes the Runtime placement fact"
    );
    assert!(persisted.session.realization.is_none(), "P1/E2");
    assert!(
        matches!(
            persisted.session.environment,
            awaken_session_state_contract::SessionEnvironmentState::Unmaterialized
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
/// latest ordinary Run is Idle; C3 it is still Running in committed Runtime;
/// C3a only its disposable Managed DTO is poisoned Running; C4 the durable archive
/// command commits Archived through the parent Session partition; C5 the same
/// request retries; C6 the disposable Managed cache is lost; C7 the id is
/// unknown; C8 the Runtime fixture exposes one already-committed coordinated
/// prefix whose root/child Messages and disposition state carry their exact
/// same-index commit coordinates before Thread assertions. Effects: E1
/// C1+C2+C4+C8 returns Terminated and emits one terminal; E2
/// C3 conflicts before the command while C3a is repaired from committed Idle;
/// E3 C5 is idempotent; E4 C6 rebuilds the same
/// terminal event id from disposition truth; E5 C7 is not found; E6 every
/// pre-terminal cursor reaches the same append-only suffix warm and cold. No child-named
/// teardown or Managed archive registry participates.
///
/// | Rule | Real | State | Archived | Retry | Cold | Result/effect |
/// |---|---|---|---|---|---|---|
/// | A1 | yes | idle | no→yes | no | no | E1,E6 |
/// | A2 | yes | running | no | no | no | E2 |
/// | A2a | yes | committed idle, warm running | no→yes | no | no | E1,E6 |
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
    let runtime = awaken_protocol_managed_testkit::CoordinatedRuntimeFake::default();
    let repo = Arc::new(ephemeral_session_repo().await);
    let state = Arc::new(
        ManagedState::new(runtime.clone())
            .await
            .with_session_repo(repo.clone()),
    );
    let request = serde_json::from_value(serde_json::json!({
        "agent": "coder", "environment_id": "env_local"
    }))
    .unwrap();
    let session = state
        .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .unwrap();
    <awaken_protocol_managed_testkit::CoordinatedRuntimeFake as SessionRuntime>::run(
        &runtime,
        "coder",
        &session.id,
        vec![ContentBlock::text("coordinate")],
    )
    .await
    .expect("A1/C8 installs one committed coordinated recovery prefix");
    state.refresh_committed_events(&session.id).await.unwrap();
    let child_id = awaken_protocol_managed_testkit::CoordinatedRuntimeFake::CHILD_THREAD_ID;
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
    let archived = state
        .archive_thread(&session.id, child_id)
        .await
        .expect("A2a committed Idle repairs the poisoned warm DTO before archive");
    assert_eq!(archived.status, SessionThreadStatus::Terminated, "A2a/E1");
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

    let restarted = ManagedState::new(runtime.clone())
        .await
        .with_session_repo(repo);
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
async fn child_thread_archive_rejects_a_committed_running_child() {
    use awaken_session_execution_contract::SessionAgentCoordination as _;

    // Causes: A2 a coordinated link and child Running lifecycle are committed;
    // its exact Run activity is admitted by the Session application. Effect:
    // archive refreshes from that source, returns Conflict and commits no
    // Archived disposition or terminal Event. A2a's warm-only Running poison
    // in the adjacent test instead refreshes back to committed Idle.
    let runtime = awaken_protocol_managed_testkit::CoordinatedRuntimeFake::default();
    runtime.defer_child_completion();
    let state = ManagedState::new(runtime.clone()).await;
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "coder", "environment_id": "env_local"
            }))
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    <awaken_protocol_managed_testkit::CoordinatedRuntimeFake as SessionRuntime>::run(
        &runtime,
        "coder",
        &session.id,
        vec![ContentBlock::text("coordinate")],
    )
    .await
    .expect("A2 committed coordination prefix");
    let admission = state
        .session_application()
        .admit_session_run_activity(
            &awaken_session_types::SessionId::from(&session.id),
            "researcher",
            &awaken_agent_contract::agent::run::Id("coord-run-1".into()),
            awaken_session_execution_contract::SessionRunActivityAdmissionMode::RecoverOrAdmit,
        )
        .await
        .unwrap();
    assert!(
        matches!(
            admission,
            awaken_session_execution_contract::SessionRunActivityAdmission::Admitted { .. }
        ),
        "A2 exact child activity admission"
    );
    state.refresh_committed_events(&session.id).await.unwrap();
    let child_id = awaken_protocol_managed_testkit::CoordinatedRuntimeFake::CHILD_THREAD_ID;
    assert_eq!(
        state.get_thread(&session.id, child_id).unwrap().status,
        SessionThreadStatus::Running,
        "A2 committed Running precondition"
    );
    assert!(
        matches!(
            state.archive_thread(&session.id, child_id).await,
            Err(StateError::Conflict)
        ),
        "A2/E2"
    );
    assert!(runtime.archive_commits().is_empty(), "A2/E2");
    assert!(
        state
            .list_thread_events(&session.id, child_id, None, None)
            .unwrap()
            .data
            .iter()
            .all(|event| event.type_str() != "session.thread_status_terminated"),
        "A2/E2 no terminal projection"
    );
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
    // coordinated prefix with exact Message commit coordinates. Effect E1 the
    // error is returned while both the disposable Thread and event stream remain
    // unchanged. Decision rule F1 is C1+C2+C3=>E1; command-success/retry/restart
    // rules are covered by the archive decision table above.
    let runtime = awaken_protocol_managed_testkit::CoordinatedRuntimeFake::default();
    let state = Arc::new(ManagedState::new(runtime.clone()).await);
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "coder", "environment_id": "env_local"
            }))
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    <awaken_protocol_managed_testkit::CoordinatedRuntimeFake as SessionRuntime>::run(
        &runtime,
        "coder",
        &session.id,
        vec![ContentBlock::text("coordinate")],
    )
    .await
    .expect("F1/C3 installs one committed coordinated recovery prefix");
    state.refresh_committed_events(&session.id).await.unwrap();
    let child_id = awaken_protocol_managed_testkit::CoordinatedRuntimeFake::CHILD_THREAD_ID;
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

/// Cause/effect graph: optional `session_thread_id` plus the one canonical
/// Runtime link/recovery prefix -> interrupt side effects. A fresh Running link
/// selects exactly itself even before the disposable Managed child cache catches
/// up; an absent selector fans out to the primary and every canonical
/// non-terminal child; an unknown selector or a canonical archived/terminated
/// child fails admission before the receipt/event log or runtime changes.
///
/// Decision table:
/// | rule | selector | target state | runtime keys | persisted receipt |
/// |---|---|---|---|---|
/// | I1 | child id | fresh Running link; cache absent | child only | yes; earlier child facts precede interrupt |
/// | I2 | primary id | live | Session id only | yes |
/// | I3 | absent | root + live child | parent partition root + child | yes |
/// | I4 | unknown/retired id | absent | none | no |
/// | I5 | absent | one target fails | every target attempted, durable receipt pending | yes |
/// | I6 | I5 retry | all targets healthy | same frozen set, processed once | same receipt |
/// | I7 | child id | canonical Archived; cache stale Running | none | no |
/// | I8 | absent | root + canonical Archived child | parent root only | yes |
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
    // selects only its stated effect and preserves the authority constraint. The shared
    // Runtime fixture first returns one root/child recovery prefix with exact
    // same-index Message commit coordinates; selector behavior never relies on
    // a read-time anchor fallback. I1 also proves the accepted interrupt uses
    // the complete committed reducer before publication: the first refresh
    // cannot insert previously unseen child facts before its issued cursor.
    let runtime = awaken_protocol_managed_testkit::CoordinatedRuntimeFake::default();
    runtime.defer_child_completion();
    let state = Arc::new(ManagedState::new(runtime.clone()).await);
    let request = serde_json::from_value(serde_json::json!({
        "agent": "coder", "environment_id": "env_local"
    }))
    .unwrap();
    let session = state
        .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .unwrap();
    <awaken_protocol_managed_testkit::CoordinatedRuntimeFake as SessionRuntime>::run(
        &runtime,
        "coder",
        &session.id,
        vec![ContentBlock::text("coordinate")],
    )
    .await
    .expect("I1-I5 setup installs one committed coordinated recovery prefix");
    let child_id = awaken_protocol_managed_testkit::CoordinatedRuntimeFake::CHILD_THREAD_ID;
    assert_eq!(
        state.list_threads(&session.id).unwrap().len(),
        1,
        "I1 precondition keeps the disposable child projection stale"
    );
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
    state.refresh_committed_events(&session.id).await.unwrap();
    let issued = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    let interrupt_position = issued
        .iter()
        .position(|event| event.type_str() == "user.interrupt")
        .expect("I1 committed interrupt receipt");
    assert!(
        issued[..interrupt_position]
            .iter()
            .any(|event| event.type_str() == "session.thread_created"),
        "I1 earlier committed child facts precede the newly accepted interrupt"
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
        .drive_session_event_batches(&SessionId::from(&session.id), None)
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
        .drive_session_event_batches(&SessionId::from(&session.id), None)
        .await
        .expect("I6 processed replay is a no-op");
    assert!(
        runtime.take_interrupts().is_empty(),
        "I6 no duplicate effect"
    );

    <awaken_protocol_managed_testkit::CoordinatedRuntimeFake as awaken_session_execution_contract::CoordinationRuntime>::archive_session_thread(
        &runtime,
        &SessionId::from(&session.id),
        child_id,
    )
    .await
    .expect("I7 canonical archive transition");
    assert_eq!(
        state.get_thread(&session.id, child_id).unwrap().status,
        SessionThreadStatus::Running,
        "I7 precondition leaves the disposable cache stale"
    );
    let interrupt_count = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data
        .iter()
        .filter(|event| event.type_str() == "user.interrupt")
        .count();
    let archived_error = state
        .send_events(
            &session.id,
            send(InboundEvent::UserInterrupt {
                session_thread_id: Some(child_id.into()),
            }),
        )
        .await
        .expect_err("I7 rejects the canonically Archived child");
    assert!(matches!(archived_error, StateError::Run(_)), "I7");
    assert!(runtime.take_interrupts().is_empty(), "I7");
    assert_eq!(
        state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .iter()
            .filter(|event| event.type_str() == "user.interrupt")
            .count(),
        interrupt_count,
        "I7 appends no rejected receipt"
    );

    state
        .send_events(
            &session.id,
            send(InboundEvent::UserInterrupt {
                session_thread_id: None,
            }),
        )
        .await
        .expect("I8 accepts the canonical non-terminal target set");
    assert_eq!(
        runtime.take_interrupts(),
        vec![format!("primary:{}", session.id)],
        "I8 excludes the canonically Archived child despite stale cache"
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
    let repo = Arc::new(ephemeral_session_repo().await);
    let state = ManagedState::new(rt).await.with_session_repo(repo.clone());
    let id = state
        .create_session(bare_create_params(), awaken_tenancy::DEFAULT_WORKSPACE_ID)
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
            repo.get(&SessionId::from(id)).await,
            Err(awaken_session_state_contract::SessionRepositoryError::NotFound)
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
    let state = ManagedState::new(rt).await;
    let id = state
        .create_session(bare_create_params(), awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .expect("create")
        .id;
    // A3 first archive observes real time; A4 idempotent replay retains the
    // original observation and cannot replace it with a later clock value.
    let first = state.archive_session(&id).await.expect("archive");
    assert_ne!(
        first.archived_at.as_deref(),
        Some(LEGACY_UNRECORDED_AT),
        "A3"
    );
    assert!(
        first
            .archived_at
            .as_deref()
            .and_then(awaken_session_application_contract::rfc3339_to_epoch_millis)
            .is_some(),
        "A3 valid time"
    );
    assert_eq!(
        *ended.lock().unwrap(),
        vec![id.clone()],
        "archive reaps the sandbox on the terminal transition"
    );
    let replay = state.archive_session(&id).await.expect("re-archive");
    assert_eq!(replay.archived_at, first.archived_at, "A4");
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
/// between child termination and aggregate Session termination; E6 every
/// terminal-relevant issued cursor remains accepted warm/cold and reaches that
/// identical terminal trio. This fixture deliberately has no retained runtime
/// interval, so pre-cutover aggregate Usage/Idle cursors are excluded here and
/// remain owned by the interval-lineage cutover tests.
///
/// | Rule | Child before archive | Parent terminal | Live | Cold | Effects |
/// |---|---|---|---|---|---|
/// | P1 | Running | yes | yes | yes | E1,E2,E3,E4,E5,E6 |
/// | P2 | Awaiting | yes | yes | yes | E1,E2,E3,E4,E5,E6 |
/// | P3 | Idle | yes | yes | yes | E1,E2,E3,E4,E5,E6 |
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
    use awaken_agent_contract::{RunLifecycleEvent, RunLifecycleEventKind};
    use awaken_session_application_contract::Pending;
    use awaken_session_execution_contract::{CoordinatedThreadLink, CoordinatedThreadTarget};

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
        let repo = Arc::new(ephemeral_session_repo().await);
        let runtime = RehydrateFake::default();
        runtime
            .delegate_ids
            .lock()
            .unwrap()
            .push("researcher".into());
        let state = Arc::new(
            ManagedState::new(runtime.clone())
                .await
                .with_session_repo(repo.clone()),
        );
        let session = state
            .create_session(
                serde_json::from_value(serde_json::json!({
                    "agent":"coder", "environment_id":"env_local"
                }))
                .unwrap(),
                awaken_tenancy::DEFAULT_WORKSPACE_ID,
            )
            .await
            .expect(rule);
        let child_id = format!("thread-parent-terminal-{rule}");
        let child_run = RunId(format!("run-parent-terminal-{rule}"));
        let coordination_call_id = format!("terminal-{rule}");
        runtime.install_agent_coordination_prefix(
            &session.id,
            RunId("root".into()),
            0,
            &coordination_call_id,
            CoordinatedThreadLink {
                session_id: SessionId::from(&session.id),
                thread_id: awaken_agent_contract::agent::thread::Id(child_id.clone()),
                target: CoordinatedThreadTarget::Agent {
                    agent_id: "researcher".into(),
                },
                created_by_operation_id: ToolBatch::operation_id_for_step(
                    &RunId("root".into()),
                    0,
                    &coordination_call_id,
                ),
                latest_run_id: Some(child_run.clone()),
            },
        );
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
        // P2 source-order constraint: production audit cursors encode the
        // backend-wide commit before the event offset; raw 1/2 would decode
        // both child transitions as commit 0 and reverse the accepted send.
        runtime.lifecycle.lock().unwrap().push(RunLifecycleEvent {
            cursor: awaken_agent_contract::encode_run_lifecycle_cursor(1, 0).unwrap(),
            source_commit_cursor: 1,
            thread_id: child_thread.clone(),
            run_id: child_run.clone(),
            kind: RunLifecycleEventKind::Running,
            state: RunState::Running,
            await_reason: None,
        });
        if let Some(kind) = terminal_kind {
            runtime.lifecycle.lock().unwrap().push(RunLifecycleEvent {
                cursor: awaken_agent_contract::encode_run_lifecycle_cursor(2, 0).unwrap(),
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
        let issued_prefix = state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data;

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
        let warm_full = state.list_events(&session.id, None, None, false).unwrap();
        assert_eq!(
            &warm_full.data[..issued_prefix.len()],
            issued_prefix.as_slice(),
            "{rule}/E6 terminal appends after the issued prefix"
        );

        let restarted = ManagedState::new(runtime.clone())
            .await
            .with_session_repo(repo.clone());
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
        let terminal_ids = vec![
            warm_child_terminal_id.clone(),
            warm_primary_terminal_id.clone(),
            warm_aggregate_terminal_id.clone(),
        ];
        for issued in issued_prefix.iter().filter(|event| {
            !matches!(
                &event.kind,
                OutboundKind::SessionUsage { .. } | OutboundKind::SessionStatusIdle { .. }
            )
        }) {
            let cursor = issued.id.as_str();
            let warm_suffix = state
                .list_events(&session.id, Some(cursor), None, false)
                .unwrap();
            let cold_suffix = restarted
                .list_events(&session.id, Some(cursor), None, false)
                .unwrap_or_else(|error| {
                    panic!(
                        "{rule}/E6 cold suffix after {} {cursor}: {error:?}",
                        issued.type_str()
                    )
                });
            let selected = |events: &[Event]| {
                events
                    .iter()
                    .filter(|event| terminal_ids.contains(&event.id))
                    .map(|event| event.id.clone())
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                selected(&warm_suffix.data),
                terminal_ids,
                "{rule}/E6 warm terminal suffix after {cursor}"
            );
            assert_eq!(
                selected(&cold_suffix.data),
                terminal_ids,
                "{rule}/E6 cold terminal suffix after {cursor}"
            );
        }
        assert!(
            runtime.archive_commits.lock().unwrap().is_empty(),
            "{rule}/E4"
        );
    }
}

#[tokio::test]
async fn archived_session_can_be_deleted_after_projection_cache_loss() {
    let repo = Arc::new(ephemeral_session_repo().await);
    let state = ManagedState::new(EndSessionRecorder::default())
        .await
        .with_session_repo(repo.clone());
    let id = state
        .create_session(bare_create_params(), awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .expect("create")
        .id;
    state.archive_session(&id).await.expect("archive");
    assert!(matches!(
        repo.get(&SessionId::from(&id)).await.unwrap().disposition,
        SessionDisposition::Archived { .. }
    ));

    let restarted = ManagedState::new(EndSessionRecorder::default())
        .await
        .with_session_repo(repo.clone());
    restarted
        .delete_session(&id)
        .await
        .expect("delete archived Session after restart");
    await_session_tombstone(repo.as_ref(), &id).await;
    assert!(
        matches!(
            repo.get(&SessionId::from(&id)).await,
            Err(awaken_session_state_contract::SessionRepositoryError::NotFound)
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
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        Arc::new(ephemeral_session_repo().await);
    let mut failed = sample_persisted("sesn_failed_delete");
    failed.execution = SessionExecutionState::ActivationFailed;
    create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, failed).await;

    let restarted = ManagedState::new(EndSessionFailer)
        .await
        .with_session_repo(repo.clone());
    restarted
        .delete_session("sesn_failed_delete")
        .await
        .expect("delete failed Session after restart");
    let pending = repo
        .get(&SessionId::from("sesn_failed_delete"))
        .await
        .unwrap();
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
    let state = ManagedState::new(runtime).await;
    let id = state
        .create_session(bare_create_params(), awaken_tenancy::DEFAULT_WORKSPACE_ID)
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
        awaken_session_application_contract::DelegatedRun {
            run_id: awaken_agent_contract::agent::run::Id("child-a".into()),
            parent_call_id: "call-a".into(),
            agent_id: "researcher".into(),
            status: awaken_agent_contract::agent::delegation::DelegationStatus::Open,
        },
        awaken_session_application_contract::DelegatedRun {
            run_id: awaken_agent_contract::agent::run::Id("child-b".into()),
            parent_call_id: "call-b".into(),
            agent_id: "reviewer".into(),
            status: awaken_agent_contract::agent::delegation::DelegationStatus::Open,
        },
        awaken_session_application_contract::DelegatedRun {
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
    // Cause/effect graph: C1 a live Session is durable; C2 its process-local
    // projection is lost; C3 archive commits the terminal fence; C4 no durable
    // resident Environment binding exists. Effects: E1 the Session is terminal;
    // E2 cold control never prepares a Runtime; E3 it invents no teardown. Rule
    // R1=C1+C2+C3+C4=>E1+E2+E3.
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        Arc::new(ephemeral_session_repo().await);
    let original = ManagedState::new(EndSessionRecorder::default())
        .await
        .with_session_repo(repo.clone());
    let id = original
        .create_session(bare_create_params(), awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .expect("create")
        .id;
    drop(original);
    let runtime = EndSessionRecorder::default();
    let ended = runtime.ended.clone();
    let prepared = runtime.prepared.clone();
    let restarted = ManagedState::new(runtime)
        .await
        .with_session_repo(repo.clone());
    let archived = restarted
        .archive_session(&id)
        .await
        .expect("archive durable Session after restart");
    assert_eq!(archived.status, SessionStatus::Terminated);
    assert!(ended.lock().unwrap().is_empty(), "E3");
    assert!(prepared.lock().unwrap().is_empty(), "E2");
    assert_eq!(
        repo.get(&SessionId::from(id)).await.unwrap().execution,
        SessionExecutionState::Terminated
    );
}

#[tokio::test]
async fn archive_session_does_not_require_a_retired_agent_publication_after_restart() {
    // Cause/effect graph: C1 durable Session freezes Agent revision 7; C2 the
    // disposable projection and exact Control publication are unavailable after
    // restart; C3 archive requests terminal cleanup; C4 no durable resident
    // Environment binding exists. Effects: E1 the durable baseline supplies
    // the control projection; E2 the Session terminates without inventing a
    // Runtime teardown; E3 ordinary interactive rehydration
    // remains fail-closed elsewhere. Requiring mutable Control presentation for
    // C3 leaks every already-terminal Run and hot-loops its downstream inbox.
    // Decision rule R1=C1+C2+C3+C4=>E1+E2+E3.
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        Arc::new(ephemeral_session_repo().await);
    let mut persisted = sample_persisted("sesn_retired_agent_cleanup");
    let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
        &persisted.baseline
    else {
        panic!("fixture baseline must be frozen");
    };
    persisted.baseline = awaken_session_configuration_contract::SessionBaselineState::Frozen(
        awaken_session_configuration_contract::SessionBaseline::compile(
            awaken_session_configuration_contract::SessionBaselineInputs {
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
    let restarted = ManagedState::new(runtime)
        .await
        .with_session_repo(repo.clone());
    let archived = restarted
        .archive_session("sesn_retired_agent_cleanup")
        .await
        .expect("terminal cleanup uses durable baseline without Control publication");
    assert_eq!(archived.status, SessionStatus::Terminated, "E1/E2");
    assert_eq!(archived.agent.version, 7, "E1 keeps the frozen revision");
    assert!(
        ended.lock().unwrap().is_empty(),
        "E2 no cleanup without a durable resident Environment"
    );
    assert!(
        prepared.lock().unwrap().is_empty(),
        "E2 never prepares a Runtime"
    );
    assert_eq!(
        repo.get(&SessionId::from("sesn_retired_agent_cleanup"))
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
        0,
        "E2 replay has no second cleanup"
    );
}

#[tokio::test]
async fn archive_persists_release_before_and_after_sandbox_teardown() {
    let repo = Arc::new(ephemeral_session_repo().await);
    let state = ManagedState::new(EndSessionRecorder::default())
        .await
        .with_session_repo(repo.clone());
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
        .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .unwrap()
        .id;
    assert_eq!(
        repo.get(&SessionId::from(&id))
            .await
            .unwrap()
            .resources
            .activations[0]
            .state,
        awaken_session_state_contract::ActivationState::Active
    );

    state.archive_session(&id).await.unwrap();
    let durable = repo.get(&SessionId::from(id)).await.unwrap();
    assert_eq!(durable.execution, SessionExecutionState::Terminated);
    assert_eq!(
        durable.resources.activations[0].state,
        awaken_session_state_contract::ActivationState::Released
    );
    assert!(
        repo.reconciliation_scan(None)
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
    let state = ManagedState::new(EndSessionRecorder::default()).await;
    assert!(
        state
            .create_session(request(500), awaken_tenancy::DEFAULT_WORKSPACE_ID)
            .await
            .is_ok()
    );
    let error = state
        .create_session(request(501), awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .unwrap_err();
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

    fn model(&self) -> String {
        "host-default-model".to_string()
    }
}

awaken_session_execution_contract::neutral_role!(BudgetRuntime for EndSessionFailer);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for EndSessionFailer);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for EndSessionFailer);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for EndSessionFailer);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for EndSessionFailer);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for EndSessionFailer {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        awaken_session_types::RunError,
    > {
        awaken_protocol_managed_testkit::observe_realization_request()
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure> {
        awaken_protocol_managed_testkit::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )?;
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Forget one failed realization fence from disposable Runtime state. The
    /// aggregate remains the sole lease authority: this hook only prevents a
    /// later retry from authorizing effects with a lease that Control has
    /// already failed or may have failed before losing its response.
    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &crate::session_resource::SessionRealizationLease,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationEffects for EndSessionFailer {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn checkpoint_session_environment(
        &self,
        thread: &str,
        request: awaken_session_execution_contract::__macro_support::SandboxCheckpointRequest,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::CheckpointReceipt,
        awaken_session_execution_contract::__macro_support::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .checkpoint_session_environment(thread, request)
            .await
    }

    async fn prepare_checkpoint_source_disposal(
        &self,
        thread: &str,
        preparation: &awaken_session_execution_contract::__macro_support::SourceReleasePreparationEffect,
        generation: &awaken_session_execution_contract::__macro_support::SandboxGeneration,
        source_binding: &str,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::SourceReleasePreparedReceipt,
        awaken_session_execution_contract::__macro_support::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_checkpoint_source_disposal(thread, preparation, generation, source_binding)
            .await
    }

    async fn dispose_prepared_checkpoint_source(
        &self,
        thread: &str,
        disposal: &awaken_session_execution_contract::__macro_support::SourceReleaseDisposal,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::SourceDisposedReceipt,
        awaken_session_execution_contract::__macro_support::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .dispose_prepared_checkpoint_source(thread, disposal)
            .await
    }

    async fn restore_checkpointed_session_environment(
        &self,
        request: awaken_session_execution_contract::__macro_support::SandboxRestoreRequest,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::RestoreReceipt,
        awaken_session_execution_contract::__macro_support::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .restore_checkpointed_session_environment(request)
            .await
    }

    async fn delete_session_checkpoint(
        &self,
        thread: &str,
        checkpoint: &awaken_session_execution_contract::__macro_support::SandboxCheckpointRef,
    ) -> Result<(), awaken_session_execution_contract::__macro_support::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .delete_session_checkpoint(thread, checkpoint)
            .await
    }

    async fn apply_session_inputs(
        &self,
        thread: &str,
        transition: &awaken_session_execution_contract::__macro_support::SessionResourceTransition,
    ) -> Result<(), awaken_session_execution_contract::__macro_support::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .apply_session_inputs(thread, transition)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Execute terminal Repository publication under the same realization
    /// generation that owns the cleanup command set and return either its
    /// secret-free receipt or one permanent compare-and-swap rejection. The
    /// caller verifies and durably records that outcome before ordinary root
    /// cleanup may dispose the shared environment. Runtimes must override this
    /// edge and carry the lease to the Repository transport boundary; falling
    /// back would erase the terminal ownership proof.
    async fn execute_terminal_repository_publication_for_lease(
        &self,
        command: awaken_session_state_contract::SessionRepositoryPublicationCommand,
        lease: &crate::session_resource::SessionRealizationLease,
    ) -> Result<
        awaken_session_state_contract::SessionRepositoryPublicationEffect,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .execute_terminal_repository_publication_for_lease(command, lease)
            .await
    }

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        _effect: awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_session_state_contract::SessionCleanupDisposalReceipt, RunError> {
        Err(RunError::internal("sandbox dispose blew up"))
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for EndSessionFailer {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one complete self-affine Session Run as an unclaimable reservation.
    /// Implementations must use their existing durable Run dispatch authority;
    /// the default fails closed rather than executing inline.
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .reserve_session_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Publish one exact reservation only after the Session root committed its
    /// activity epoch. Ordinary Worker claim/execution remains the sole runner.
    async fn activate_session_run(
        &self,
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .activate_session_run(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Register a foreground observer before publishing a delivery, then wait
    /// for the exact committed Run to become `Awaiting` or `Ended`.
    ///
    /// The admission plan is produced only after the Session application has
    /// committed its exact activity receipt. Recovery-only plans still wait on
    /// committed Thread truth and never create another reservation/activity.
    /// Public queued Event acceptance has its own durable command boundary and
    /// does not wait through this foreground observation port.
    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one ordinary child Run after the Session application has validated
    /// the frozen roster and lifecycle. Implementations must use their canonical
    /// durable Run ingress; the default fails closed rather than spawning a
    /// process-local task.
    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Subscribe to neutral live observations for one already-admitted root or
    /// child logical Thread. `None` is the fail-closed default: committed events
    /// remain available, but no Runtime without an exact Thread-scoped observer
    /// can accidentally leak previews across Threads. This port is a view over
    /// the Runtime's existing live hub, not another fan-out or replay owner.
    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one Session-approved child report as immutable input of the
    /// deterministic primary Run. Implementations must not stage a parallel
    /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
    async fn continue_session_agent_report(
        &self,
        command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .continue_session_agent_report(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt one logical child through its parent Session's physical commit
    /// and dispatch partition. Implementations must not open child-named storage.
    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .reply_session_thread_tool(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
    /// reply ingress remains identical; implementations additionally register
    /// before publication and project the next committed Step from Thread
    /// truth. No inline executor is authorized by this observation policy.
    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await
    }

    async fn quiesce_terminal_delegations(
        &self,
        _thread: &str,
    ) -> Result<awaken_session_application_contract::DelegatedRunSnapshot, RunError> {
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: Vec::new(),
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: 0,
        })
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionCoordinationReads for EndSessionFailer {
    async fn source_dispatch_observation(
        &self,
        _session: &awaken_session_state_contract::PersistedSession,
    ) -> awaken_session_execution_contract::SessionSourceDispatchObservation {
        Default::default()
    }

    async fn coordinated_admission_receipt(
        &self,
        command: awaken_session_execution_contract::CoordinatedAdmissionRef<'_>,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionAgentMessageReceipt>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_admission_receipt(command)
            .await
    }

    /// Read the exact Run lifecycle from existing committed Thread truth. This
    /// is the recovery observation for batch advancement, not a completion
    /// registry or protocol event cache.
    async fn session_run_state(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_run_state(session_id, run_id)
            .await
    }

    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_session_application_contract::DelegatedRun>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

    /// Rebuild the Session's coordinated child-Thread links from existing
    /// committed ToolBatch/transcript/dispatch facts. This is a query, not a
    /// relationship repository.
    /// `observed_root` may borrow an atomic prefix already read for this Session
    /// during a larger read-only projection. Implementations must reject a
    /// different root identity and fence enrichment to that prefix. `None`
    /// reads the current prefix; command admission and final validation use it.
    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<
        Vec<awaken_session_execution_contract::CoordinatedThreadLink>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_threads(session_id, observed_root)
            .await
    }

    /// Read and validate the exact committed Awaiting coordinate that a Primary
    /// or child Thread tool reply intends to answer. The command already carries
    /// its admission-time Run/correlation identity; this returns only the current
    /// dispatch activity coordinate. Implementations revalidate the command when
    /// applying the reply so a concurrent resume or later Run fails closed.
    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionThreadToolReplyFence,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_tool_reply_fence(command)
            .await
    }

    /// Validate one model-facing Agent command against the source Run's exact
    /// committed `ActiveToolBatch`. Implementations must use the same atomic
    /// Thread recovery prefix as ordinary crash recovery; a process-local tool
    /// context or authenticated Worker claim alone is not sufficient proof of
    /// call identity or payload. Success returns the command reconstructed from
    /// that committed prefix; Session policy consumes that value instead of
    /// trusting a second copy supplied by the caller. The fail-closed default
    /// keeps lightweight runtimes from silently becoming a second trusted
    /// command source.
    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageCommand,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for EndSessionFailer {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(&self, thread: &str) -> Result<bool, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The committed transcript for `thread`, in commit order. Used to rehydrate a
    /// session whose in-memory record was lost (e.g. after a process restart) from
    /// durable truth: a non-empty result means the thread exists in the store. The
    /// default reports nothing, so an ephemeral host never rehydrates.
    async fn committed_messages(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_agent_contract::agent::message::Message>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .committed_messages(thread)
            .await
    }

    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &SessionId,
        _thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        Ok(None)
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::select_recovery_snapshot_run(
            self.session_thread_recovery_snapshot(session_id, thread_id)
                .await?,
            run_id,
        )
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`awaken_session_application_contract::StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::TerminalCleanupRuntime for EndSessionFailer {
    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RunError> {
        awaken_protocol_managed_testkit::complete_terminal_cleanup_preparation(
            &effect,
            &authorization,
        )
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only a prepared child projection after Control durably accepts
    /// its exact preparation. The shared root Environment must remain until the
    /// aggregate-wide physical disposal is complete.
    async fn acknowledge_terminal_cleanup_preparation(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_preparation(effect)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only the process-local teardown projection after Control has
    /// durably accepted the exact disposal receipt. No provider or Resource
    /// effect is permitted at this edge.
    async fn acknowledge_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_disposal(effect)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire a process-local terminal projection after Control reports that
    /// the aggregate has no remaining cleanup work. This closes final receipt
    /// response loss without replaying provider I/O or inventing another
    /// completion registry.
    async fn acknowledge_completed_terminal_cleanup(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &crate::session_resource::SessionRealizationLease,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_completed_terminal_cleanup(session_id, lease)
            .await
    }

    async fn install_terminal_cleanup_assignment(
        &self,
        _assignment: &awaken_session_realization_contract::SessionTerminalCleanupAssignment,
        _proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for EndSessionFailer {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one Outcome aggregate without executing a Worker or Grader Run.
    /// The opaque id is supplied by the application command so crash replay and
    /// activity admission share one stable operation identity.
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(
        &self,
        thread: &str,
    ) -> Result<Option<awaken_session_execution_contract::OutcomeDrive>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
    }
}

/// Delete-cleanup cause/effect graph: C1 terminal quiescence proves there are no
/// child targets; C2 the durable cleanup intent moves the active Resource to
/// Releasing; C3 Runtime teardown fails. Effects: E1 DELETE remains accepted and
/// public reads are 404; E2 the durable Session remains Deleting+Releasing for
/// the existing ResourceReclaimer; E3 it remains in the reconcilable index.
/// Decision rule D2=C1+C2+C3=>E1+E2+E3. The teardown error is retriable cleanup
/// state, not authority to resurrect the public Session.
#[tokio::test]
async fn delete_is_best_effort_when_sandbox_teardown_fails() {
    let repo = Arc::new(ephemeral_session_repo().await);
    let state = ManagedState::new(EndSessionFailer)
        .await
        .with_session_repo(repo.clone());
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
        .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
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
            let durable = repo.get(&SessionId::from(&id)).await.unwrap();
            if durable.resources.activations[0].state
                == awaken_session_state_contract::ActivationState::Releasing
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
        awaken_session_state_contract::ActivationState::Releasing,
        "cleanup failure stays durable for ResourceReclaimer"
    );
    assert_eq!(
        repo.reconciliation_scan(None).await.unwrap().sessions,
        vec![awaken_session_state_contract::ScopedPersistedSession {
            workspace_id: DEFAULT_SCOPE.to_string(),
            session: durable,
        }]
    );
}

/// Archive and physical teardown are separate consistency boundaries. Once the
/// durable Archived fence wins, a transient Runtime cleanup failure remains a
/// lifecycle-recoverable effect instead of changing the command result.
#[tokio::test]
async fn archive_commits_while_sandbox_teardown_remains_recoverable() {
    let repo = Arc::new(ephemeral_session_repo().await);
    let state = ManagedState::new(EndSessionFailer)
        .await
        .with_session_repo(repo.clone());
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
        .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .expect("create")
        .id;

    let archived = state
        .archive_session(&id)
        .await
        .expect("the durable archive fence is independent from cleanup availability");
    assert_eq!(archived.status, SessionStatus::Terminated);

    let durable = repo.get(&SessionId::from(id)).await.unwrap();
    assert!(matches!(
        durable.disposition,
        SessionDisposition::Archived { .. }
    ));
    assert_eq!(
        durable.resources.activations[0].state,
        awaken_session_state_contract::ActivationState::Releasing,
        "failed cleanup remains visible to lifecycle recovery"
    );
    assert_eq!(
        repo.reconciliation_scan(None).await.unwrap().sessions,
        vec![awaken_session_state_contract::ScopedPersistedSession {
            workspace_id: DEFAULT_SCOPE.to_string(),
            session: durable,
        }]
    );
}

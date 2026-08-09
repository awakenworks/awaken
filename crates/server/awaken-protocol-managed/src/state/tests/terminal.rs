use super::*;

#[tokio::test]
async fn coordinator_only_creation_does_not_report_success_before_worker_acknowledgement() {
    // Cause/effect graph: C1 placement is a registered Worker; C2 the durable
    // creation intent and dispatch succeed; C3 no Worker acknowledges before
    // the readiness deadline. Effects: E1 the public create fails unavailable;
    // E2 the durable aggregate remains Preparing with Worker placement; E3 no
    // initial-idle lifecycle fact exists; E4 Control performs no local
    // realization. Decision table: R1 C1+C2+C3 => E1+E2+E3+E4. FMECA: the old
    // Preparing-as-200 path could advertise readiness while no Worker owned the
    // demand (severity 9, occurrence 5, detection 7); the readiness barrier
    // makes that state observable as a retryable failure instead of success.
    let runtime = EndSessionRecorder::default();
    let prepared = runtime.prepared.clone();
    let runtime = Arc::new(runtime);
    let repo = Arc::new(ephemeral_session_repo());
    let application = awaken_session_application::SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(mcp_attachment::UnsupportedMcpAttachmentRealizer),
        repo.clone(),
        crate::test_support::environment_components().1,
        awaken_session_application::SessionApplicationConfiguration {
            execution_placement:
                awaken_session_application::SessionExecutionPlacement::RegisteredWorker,
            create_readiness_timeout: std::time::Duration::from_millis(20),
            create_readiness_poll_interval: std::time::Duration::from_millis(1),
            ..Default::default()
        },
    );
    let state = ManagedState::from_application(application);
    let error = state
        .create_session(
            serde_json::from_value(serde_json::json!({ "agent": "assistant" })).unwrap(),
            None,
        )
        .await
        .expect_err("R1/E1 readiness is not acknowledged");
    assert!(
        matches!(
            error,
            StateError::Run(RunError {
                kind: awaken_session_contract::RunErrorKind::Unavailable,
                ..
            })
        ),
        "R1/E1: {error}"
    );
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

/// Cause graph: exact child -> runtime termination -> one terminal projection.
/// Runtime failure stops before status/event mutation; a repeated successful
/// archive observes the terminal projection and performs no second effect.
///
/// | Child | Runtime | Prior status | Result | Runtime calls | terminal events |
/// |---|---|---|---|---|---|
/// | absent | n/a | n/a | 404 | 0 | 0 |
/// | present | fail | running | error, still running | 1 | 0 |
/// | present | succeed | running | terminated | 1 | 1 |
/// | present | n/a | terminated | same receipt | unchanged | unchanged |
#[tokio::test]
async fn child_thread_archive_is_runtime_backed_fail_closed_and_idempotent() {
    let runtime = EndSessionRecorder::default();
    let ended = runtime.ended.clone();
    let state = ManagedState::new(runtime);
    let request = serde_json::from_value(serde_json::json!({ "agent": "assistant" })).unwrap();
    let session = state.create_session(request, None).await.unwrap();
    {
        let mut sessions = state.sessions.lock().unwrap();
        let record = sessions.get_mut(&session.id).unwrap();
        record.child_threads.push(ManagedState::child_thread(
            &record.session,
            "child-1",
            "researcher",
        ));
    }

    let archived = state.archive_thread(&session.id, "child-1").await.unwrap();
    assert_eq!(archived.status, SessionThreadStatus::Terminated);
    assert_eq!(ended.lock().unwrap().as_slice(), &["child-1"]);
    let terminal_count = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data
        .iter()
        .filter(|event| event.type_str() == "session.thread_status_terminated")
        .count();
    assert_eq!(terminal_count, 1);

    state.archive_thread(&session.id, "child-1").await.unwrap();
    assert_eq!(ended.lock().unwrap().as_slice(), &["child-1"]);
    assert_eq!(
        state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .iter()
            .filter(|event| event.type_str() == "session.thread_status_terminated")
            .count(),
        1
    );
}

/// Cause/effect graph: optional `session_thread_id` -> canonical runtime
/// Thread selection -> interrupt side effects. A named live Thread selects
/// exactly itself; an absent selector fans out to the primary and every
/// non-terminal child; an unknown or terminal selector fails admission before
/// the receipt/event log or runtime changes.
///
/// Decision table:
/// | rule | selector | target state | runtime keys | persisted receipt |
/// |---|---|---|---|---|
/// | I1 | child id | idle/requires-action | child only | yes |
/// | I2 | primary id | live | Session id only | yes |
/// | I3 | absent | mixed | primary + non-terminal children | yes |
/// | I4 | child id | terminated/unknown | none | no |
#[tokio::test]
async fn interrupt_selector_targets_one_thread_or_all_non_terminal_threads() {
    let runtime = EndSessionRecorder::default();
    let interrupted = runtime.interrupted.clone();
    let state = ManagedState::new(runtime);
    let request = serde_json::from_value(serde_json::json!({ "agent": "assistant" })).unwrap();
    let session = state.create_session(request, None).await.unwrap();
    {
        let mut sessions = state.sessions.lock().unwrap();
        let record = sessions.get_mut(&session.id).unwrap();
        let mut idle = ManagedState::child_thread(&record.session, "child-idle", "researcher");
        idle.status = SessionThreadStatus::Idle;
        record.child_threads.push(idle);
        let mut terminated = ManagedState::child_thread(&record.session, "child-ended", "reviewer");
        terminated.status = SessionThreadStatus::Terminated;
        terminated.archived_at = Some(PROCESSED_AT.to_string());
        record.child_threads.push(terminated);
    }

    let send = |event| SendEventsRequest {
        events: vec![event],
        user_profile_id: None,
    };
    state
        .send_events(
            &session.id,
            send(InboundEvent::UserInterrupt {
                session_thread_id: Some("child-idle".into()),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        interrupted.lock().unwrap().as_slice(),
        &["child-idle"],
        "I1"
    );
    assert_eq!(
        state
            .list_thread_events(&session.id, "child-idle", None, None)
            .unwrap()
            .data
            .iter()
            .filter(|event| event.type_str() == "user.interrupt")
            .count(),
        1,
        "I1 is visible on the selected child Thread stream"
    );

    interrupted.lock().unwrap().clear();
    state
        .send_events(
            &session.id,
            send(InboundEvent::UserInterrupt {
                session_thread_id: Some(format!("{}:primary", session.id)),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        interrupted.lock().unwrap().as_slice(),
        &[session.id.as_str()],
        "I2"
    );

    interrupted.lock().unwrap().clear();
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
        interrupted.lock().unwrap().as_slice(),
        &[session.id.as_str(), "child-idle"],
        "I3 excludes the terminal child"
    );
    assert_eq!(
        state
            .list_thread_events(&session.id, "child-idle", None, None)
            .unwrap()
            .data
            .iter()
            .filter(|event| event.type_str() == "user.interrupt")
            .count(),
        2,
        "I3's selector-free interrupt is visible on every live child stream"
    );

    for rejected in ["child-ended", "child-unknown"] {
        interrupted.lock().unwrap().clear();
        let event_count = state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .len();
        let error = state
            .send_events(
                &session.id,
                send(InboundEvent::UserInterrupt {
                    session_thread_id: Some(rejected.into()),
                }),
            )
            .await
            .expect_err("I4 rejects a non-live selector");
        assert!(matches!(error, StateError::Run(_)), "I4: {error}");
        assert!(interrupted.lock().unwrap().is_empty(), "I4");
        assert_eq!(
            state
                .list_events(&session.id, None, None, false)
                .unwrap()
                .data
                .len(),
            event_count,
            "I4 admission is atomic"
        );
    }
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

/// `POST /v1/sessions/{id}/archive` reaps the sandbox on the terminal
/// transition only — a re-archive (idempotent) does not re-dispose.
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
            "researcher",
        ));
        record.child_threads.push(ManagedState::child_thread(
            &record.session,
            "child-b",
            "reviewer",
        ));
        record.child_threads.push(ManagedState::child_thread(
            &record.session,
            "child-a",
            "duplicate-projection",
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
async fn archive_persists_release_before_and_after_sandbox_teardown() {
    let repo = Arc::new(ephemeral_session_repo());
    let state = ManagedState::new(EndSessionRecorder::default()).with_session_repo(repo.clone());
    let request = serde_json::from_value(serde_json::json!({
        "agent": "assistant",
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
    async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
        Ok(())
    }
    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<OutcomeReport, RunError> {
        unreachable!()
    }
    async fn end_session(&self, _thread: &str) -> Result<(), RunError> {
        Err(RunError::internal("sandbox dispose blew up"))
    }
    fn model(&self) -> String {
        "host-default-model".to_string()
    }
}

#[tokio::test]
async fn child_thread_archive_failure_commits_no_terminal_projection() {
    let state = ManagedState::new(EndSessionFailer);
    let request = serde_json::from_value(serde_json::json!({ "agent": "assistant" })).unwrap();
    let session = state.create_session(request, None).await.unwrap();
    {
        let mut sessions = state.sessions.lock().unwrap();
        let record = sessions.get_mut(&session.id).unwrap();
        record.child_threads.push(ManagedState::child_thread(
            &record.session,
            "child-fails",
            "researcher",
        ));
    }

    let error = state
        .archive_thread(&session.id, "child-fails")
        .await
        .expect_err("runtime failure must fail closed");
    assert!(error.to_string().contains("sandbox dispose blew up"));
    assert_eq!(
        state.get_thread(&session.id, "child-fails").unwrap().status,
        SessionThreadStatus::Running
    );
    assert!(
        !state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .iter()
            .any(|event| event.type_str() == "session.thread_status_terminated")
    );
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
    let durable = repo.get(&id).await.unwrap();
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
        sandbox: serde_json::json!({"isolation": "namespace"}),
        sandbox_provisioning: Default::default(),
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
                    model: "kimi-k2".into(),
                    runtime: Some("acp:custom".into()),
                    application: None,
                    delegate_ids: Vec::new(),
                    toolsets: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                },
            ),
        ),
        title: Some("My session".to_string()),
        metadata,
        tools: Default::default(),
        activity_epoch: 0,
        environment: Default::default(),
        mcp,
        resources: awaken_session_contract::SessionResourceState::from_legacy(sample_inputs()),
        realization: None,
        realization_progress: Default::default(),
        execution: SessionExecutionState::Idle,
        disposition: Default::default(),
        terminal_cleanup: Default::default(),
    }
}

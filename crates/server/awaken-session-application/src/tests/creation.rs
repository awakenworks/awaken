use super::*;

struct AdmissionEnvironment;

#[async_trait::async_trait]
impl SessionEnvironmentSource for AdmissionEnvironment {
    async fn get(
        &self,
        _environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError> {
        Ok(None)
    }

    async fn resolve_current_for_session(
        &self,
        _environment_id: &str,
        _runtime: Option<&str>,
        _mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
        Ok(Some(ResolvedSessionEnvironment {
            snapshot: persisted("environment-template", false, false, "idle")
                .frozen_baseline()
                .expect("fixture baseline")
                .environment
                .clone(),
        }))
    }

    async fn resolve_exact_for_session(
        &self,
        environment_id: &str,
        _revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
        self.resolve_current_for_session(environment_id, runtime, mcp_targets)
            .await
    }

    async fn enqueue_session_work(
        &self,
        _environment_id: &str,
        _session_id: &str,
    ) -> Result<String, awaken_session_contract::work_queue::WorkQueueError> {
        unreachable!("local admission does not dispatch WorkQueue work")
    }
}

fn creation_command(
    session_id: &str,
    application: awaken_session_contract::ApplicationContributionState,
) -> CreateSessionCommand {
    let environment = persisted("environment-template", true, false, "idle")
        .frozen_baseline()
        .expect("fixture baseline")
        .environment
        .clone();
    CreateSessionCommand {
        owner_scope: "workspace".into(),
        session_id: session_id.into(),
        intent: awaken_session_contract::SessionCreationIntent {
            control: awaken_session_contract::ControlSessionCreationInputs {
                environment,
                runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
                agent_id: "agent".into(),
                model: "model".into(),
                execution_model_ref: "model".into(),
                runtime: None,
                mcp_authoring: Default::default(),
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                resources: Default::default(),
                initial_mcp: Vec::new(),
            },
            application,
        },
        title: None,
        metadata: Default::default(),
        tools: Default::default(),
    }
}

#[tokio::test]
async fn creation_driver_owns_finalize_realize_and_activation_order() {
    // Cause/effect graph: C1 contribution is absent or required; C2 the frozen
    // placement is local; C3 durable insertion succeeds; C4 realization is
    // acknowledged. Effects: E1 absent is finalized before realization and
    // becomes Idle; E2 required remains a Preparing intent and performs no
    // realization; E3 both identities have one owner in the same repository;
    // E4 only the acknowledged Session owns the durable initial-idle fact.
    // Decision table: R1 !C1-required+C2+C3+C4 => E1+E3+E4; R2
    // C1-required+C2+C3 => E2+E3+!E4. FMECA: emitting E4 at intent insertion
    // creates a false-ready fact if realization later fails, severity 9,
    // occurrence 4, detection 7; the acknowledgement boundary removes that
    // failure mode. This proves protocols cannot reorder the shared sequence.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let app = application(
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let active = app
        .create_session(creation_command(
            "active",
            awaken_session_contract::ApplicationContributionState::Absent,
        ))
        .await
        .expect("R1");
    assert!(active.frozen_baseline().is_some(), "R1/E1");
    assert_eq!(
        active.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "R1/E1"
    );

    let preparing = app
        .create_session(creation_command(
            "preparing",
            awaken_session_contract::ApplicationContributionState::Required,
        ))
        .await
        .expect("R2");
    assert!(
        matches!(
            preparing.baseline,
            awaken_session_contract::SessionBaselineState::Preparing(_)
        ),
        "R2/E2"
    );
    assert_eq!(
        preparing.execution,
        awaken_session_contract::SessionExecutionState::Preparing,
        "R2/E2"
    );
    assert_eq!(
        repository.owner("active").await.unwrap(),
        "workspace",
        "R1/E3"
    );
    assert_eq!(
        repository.owner("preparing").await.unwrap(),
        "workspace",
        "R2/E3"
    );
    let pending = repository.pending_lifecycle().await.unwrap();
    assert_eq!(pending.len(), 1, "R1/E4 + R2/!E4");
    assert_eq!(pending[0].object_id, "active", "R1/E4");
    assert_eq!(pending[0].event_type, "session.status_idled", "R1/E4");
}

#[tokio::test]
async fn self_hosted_work_dispatch_is_durable_but_not_a_fabricated_readiness_ack() {
    // Environment WorkQueue FMECA/cause-effect graph. C1 the frozen Environment
    // is self-hosted; C2 Runtime placement is a registered Worker; C3 the
    // idempotent WorkQueue projection succeeds; C4 no external worker has yet
    // acknowledged physical realization. Effects: E1 create returns the
    // durable Preparing projection without waiting for an acknowledgement that
    // this protocol does not carry; E2 exactly one stable Session work item is
    // queued; E3 no false Idle lifecycle fact is committed. Treating C3 as C4
    // previously caused every split-process create to time out (S9/O10/D2).
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // | W1   | T  | T  | T  | F  | E1+E2+E3 |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let app = application_with_configuration(
        repository.clone(),
        environments.clone(),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let mut command = creation_command(
        "external-create",
        awaken_session_contract::ApplicationContributionState::Absent,
    );
    command.intent.control.runtime_placement =
        awaken_session_contract::SessionRuntimePlacement::Worker;

    let created = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.create_session(command),
    )
    .await
    .expect("W1/E1 create does not invent a physical-ack barrier")
    .expect("W1/E1 durable create succeeds");
    assert_eq!(
        created.execution,
        awaken_session_contract::SessionExecutionState::Preparing,
        "W1/E1+E3"
    );
    assert_eq!(
        environments
            .dispatched
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        ["external-create".to_string()],
        "W1/E2"
    );
    assert!(
        repository.pending_lifecycle().await.unwrap().is_empty(),
        "W1/E3"
    );
}

#[tokio::test]
async fn session_application_admits_new_and_existing_protocol_threads() {
    // Cause/effect graph: C1 durable thread absent/present; C2 workspace matches
    // the durable owner; C3 Agent is available. Effects: E1 absent creates one
    // frozen Session; E2 present reuses and realizes that same aggregate; E3 an
    // owner mismatch fails before Runtime execution. Decision table: A1 absent+
    // C3 => E1; A2 present+C2 => E2; A3 present+!C2 => E3. This is the concrete
    // application gate shared by AI SDK, AG-UI, and A2A.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let app = application(repository.clone(), Arc::new(AdmissionEnvironment));

    app.admit_run_session("workspace", "thread", "assistant")
        .await
        .expect("A1");
    let created = repository.get("thread").await.expect("A1/E1");
    assert!(created.frozen_baseline().is_some(), "A1/E1");
    assert_eq!(created.agent_id(), Some("assistant"), "A1/E1");

    app.admit_run_session("workspace", "thread", "ignored")
        .await
        .expect("A2/E2");
    let error = app
        .admit_run_session("other-workspace", "thread", "ignored")
        .await
        .expect_err("A3/E3");
    assert_eq!(
        error.kind,
        awaken_session_contract::RunErrorKind::BadRequest,
        "A3/E3"
    );
}

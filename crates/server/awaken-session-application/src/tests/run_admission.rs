use super::*;
use awaken_session_contract::{
    AdmitSessionRun, AdmittedSessionRun, SessionRunActivation, SessionRunDelivery,
    SessionRunReservation, StepOutcome,
};

struct ProtocolProjectionRuntime;

#[derive(Default)]
struct ActivationOnlySessionRunRuntime {
    reservations: Mutex<Vec<AdmitSessionRun>>,
    deliveries: Mutex<Vec<SessionRunDelivery>>,
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionRuntime for ActivationOnlySessionRunRuntime {
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        install_complete_test_projection(self, thread, projection, mode).await
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("activation-only test never executes inline")
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("activation-only test never resumes")
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("activation-only test never resumes custom input")
    }

    async fn activate_session_run(
        &self,
        delivery: SessionRunDelivery,
    ) -> Result<SessionRunActivation, RunError> {
        self.deliveries.lock().unwrap().push(delivery);
        Ok(SessionRunActivation::Activated)
    }

    async fn reserve_session_run(
        &self,
        command: AdmitSessionRun,
    ) -> Result<SessionRunReservation, RunError> {
        self.reservations.lock().unwrap().push(command);
        Ok(SessionRunReservation::Reserved)
    }

    fn model(&self) -> String {
        "activation-only-session-run".into()
    }
}

#[tokio::test]
async fn background_attention_uses_canonical_session_admission_without_observing() {
    // Cause/effect decision table: C1 a durable Session exists; C2 the caller
    // requests background observation; C3 reservation succeeds; C4 the exact
    // Session activity receipt commits. Effects: E1 one PreservePrior command
    // is reserved; E2 the same Run id and non-zero epoch are activated once;
    // E3 the caller can return while the Session remains Running; E4 a
    // BackgroundTask reminder remains ordinary Role::System Run input rather
    // than Session state or a special notification payload. Failure rule B2:
    // C3 or C4 false => no executable delivery. Constraint K1: neither Host
    // enqueue nor a second background lifecycle is available to this port.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        repository.as_ref(),
        persisted("background-protocol", false, "idle"),
    )
    .await;
    let runtime = Arc::new(ActivationOnlySessionRunRuntime::default());
    let sessions = Arc::new(application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let protocol = SessionRunApplication::new(
        Arc::new(ProtocolProjectionRuntime),
        sessions,
        |_| "workspace".into(),
        |_| Some("agent".into()),
    );
    let run_id =
        awaken_session_contract::SessionRunBackgroundApplication::submit_session_run_background(
            &protocol,
            "background-operation",
            "background-protocol",
            None,
            vec![awaken_agent_contract::agent::message::Message::text(
                awaken_agent_contract::agent::message::Id("background-input".into()),
                awaken_agent_contract::agent::message::Role::System,
                "inspect the completed background task",
            )],
            None,
        )
        .await
        .expect("B1 background admission");

    {
        let reservations = runtime.reservations.lock().unwrap();
        assert_eq!(reservations.len(), 1, "B1/E1");
        assert_eq!(reservations[0].run_id, run_id, "B1/E1 exact Run");
        assert_eq!(
            reservations[0].messages[0].role,
            awaken_agent_contract::agent::message::Role::System,
            "B1/E4 system attention input",
        );
        assert_eq!(
            reservations[0].replacement,
            awaken_session_contract::SessionRunReplacement::PreservePrior,
            "B1/E1 append semantics",
        );
    }
    {
        let deliveries = runtime.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1, "B1/E2");
        assert_eq!(deliveries[0].run_id, run_id, "B1/E2 exact Run");
        assert!(deliveries[0].session_activity_epoch > 0, "B1/E2 receipt");
    }
    assert_eq!(
        repository
            .get("background-protocol")
            .await
            .expect("B1 durable Session")
            .execution,
        SessionExecutionState::Running,
        "B1/E3 background caller did not wait for settlement",
    );
}

struct BlockingSessionRunRuntime {
    entered: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
    application: std::sync::OnceLock<std::sync::Weak<SessionApplication>>,
}

#[async_trait::async_trait]
impl awaken_session_contract::RunApplication for ProtocolProjectionRuntime {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<awaken_agent_contract::agent::message::Message>,
    ) -> Result<awaken_session_contract::StepOutcome, awaken_session_contract::RunError> {
        unreachable!("Session Run execution is owned by SessionRuntime")
    }

    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: awaken_session_contract::RunResume,
    ) -> Result<awaken_session_contract::StepOutcome, awaken_session_contract::RunError> {
        unreachable!("protocol activity test never resumes")
    }

    async fn pending(
        &self,
        _thread: &str,
    ) -> Result<Option<awaken_session_contract::Pending>, awaken_session_contract::RunError> {
        Ok(None)
    }

    async fn history(
        &self,
        _thread: &str,
    ) -> Result<
        Vec<awaken_agent_contract::agent::message::Message>,
        awaken_session_contract::RunError,
    > {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "protocol-projection".into()
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionRuntime for BlockingSessionRunRuntime {
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        install_complete_test_projection(self, thread, projection, mode).await
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("legacy inline execution is not an admission path")
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("test never resumes")
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("test never resumes a custom tool")
    }

    async fn reserve_session_run(
        &self,
        _command: AdmitSessionRun,
    ) -> Result<SessionRunReservation, RunError> {
        Ok(SessionRunReservation::Reserved)
    }

    async fn activate_and_observe_session_run(
        &self,
        admission: AdmittedSessionRun,
        _input_message_ids: Vec<String>,
        _sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        let delivery = admission
            .delivery()
            .expect("fresh test admission carries the durable activity receipt");
        self.entered.add_permits(1);
        self.release
            .acquire()
            .await
            .expect("test release semaphore")
            .forget();
        self.application
            .get()
            .and_then(std::sync::Weak::upgrade)
            .expect("test Session application")
            .settle_activity(&delivery.session_id, delivery.session_activity_epoch)
            .await
            .expect("committed Run observation settles the exact activity");
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    fn model(&self) -> String {
        "blocking-session-run".into()
    }
}

#[tokio::test]
async fn background_activation_uses_the_session_runtime_without_a_second_queue_port() {
    // Cause/effect graph: C1 admission returns a delivery, recovery-claimed, or
    // completed; C2 the caller wants asynchronous observation. Effects: E1 an
    // exact delivery crosses the SessionRuntime activation port once; E2 closed
    // recovery/terminal outcomes are projected without touching Runtime.
    // Constraint: SessionApplication exposes no dispatch repository or raw row.
    // Decision table: R1=C1.delivery+C2=>E1; R2=C1.recovery=>E2.RecoveryClaimed;
    // R3=C1.completed=>E2.Completed.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let runtime = Arc::new(ActivationOnlySessionRunRuntime::default());
    let application = application_with_runtime(
        runtime.clone(),
        repository,
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let delivery = SessionRunDelivery {
        session_id: "background-session".into(),
        run_id: awaken_agent_contract::agent::run::Id("background-run".into()),
        session_activity_epoch: 7,
    };
    assert_eq!(
        application
            .activate_admitted_session_run(AdmittedSessionRun::Reserved(delivery.clone()))
            .await
            .expect("R1 activation"),
        SessionRunActivation::Activated,
        "R1/E1"
    );
    assert_eq!(&*runtime.deliveries.lock().unwrap(), &[delivery], "R1/E1");
    assert_eq!(
        application
            .activate_admitted_session_run(AdmittedSessionRun::RecoveryClaimed {
                session_id: "background-session".into(),
                run_id: awaken_agent_contract::agent::run::Id("background-run".into()),
            })
            .await
            .expect("R2 projection"),
        SessionRunActivation::RecoveryClaimed,
        "R2/E2"
    );
    assert_eq!(runtime.deliveries.lock().unwrap().len(), 1, "R2/E2");
    assert_eq!(
        application
            .activate_admitted_session_run(AdmittedSessionRun::Completed {
                session_id: "background-session".into(),
                run_id: awaken_agent_contract::agent::run::Id("background-run".into()),
            })
            .await
            .expect("R3 projection"),
        SessionRunActivation::Completed,
        "R3/E2"
    );
    assert_eq!(runtime.deliveries.lock().unwrap().len(), 1, "R3/E2");
}

#[tokio::test]
async fn admission_surfaces_dispatch_failure_and_preserves_retryable_intent() {
    // Session-realization dispatch FMECA and cause/effect graph. C1 the frozen
    // Session requires a registered Worker; C2 the durable dispatch projection
    // is installed; C3 the WorkQueue write succeeds or fails; C4 the same
    // durable intent is retried after the queue recovers. Effects: E1 a failed
    // first dispatch returns the stable `session_work_dispatch_failed` error
    // before Runtime execution; E2 the Session remains reconcilable (no false
    // Idle/terminal transition); E3 retry uses the same Session identity and
    // dispatches exactly once. Critical failure mode: swallowing C3 previously
    // left the caller waiting with no immediate cause while appearing admitted.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // | D1   | T  | T  | F  | F  | E1+E2  |
    // | D2   | T  | T  | T  | T  | E3     |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    create(repo.as_ref(), persisted("dispatch-admission", true, "idle")).await;
    environments.fail_for("dispatch-admission");
    let refresh = Arc::new(ToggleProjectionRefresh::default());
    refresh.fail.store(true, Ordering::SeqCst);
    let mut application = application(repo.clone(), environments.clone());
    application
        .set_executable_projection_refresh(refresh.clone())
        .unwrap();

    // Projection prerequisite decision extension: C5 refresh unavailable;
    // E4 recovery fails before Resource/Runtime/WorkQueue effects; C6 retry
    // after recovery; E5 the existing canonical dispatch path runs. Constraint
    // K1 durable Session intent remains unchanged. D3 C5=>E4; D4 C6=>E5.
    let revision = repo.get("dispatch-admission").await.unwrap().revision;
    assert!(
        matches!(
            application
                .recover_session_projection("dispatch-admission", Some("workspace"))
                .await,
            Err(SessionProjectionRecoveryError::Unavailable(_))
        ),
        "D3/E4"
    );
    assert_eq!(
        repo.get("dispatch-admission").await.unwrap().revision,
        revision,
        "D3/E4 no durable mutation"
    );
    assert!(environments.dispatched.lock().unwrap().is_empty(), "D3/E4");
    refresh.fail.store(false, Ordering::SeqCst);

    let error = match application
        .recover_session_projection("dispatch-admission", Some("workspace"))
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("D1/E1 queue failure must reject current admission"),
    };
    let SessionProjectionRecoveryError::Rejected(error) = error else {
        panic!("D1/E1 must preserve the classified Run error");
    };
    assert_eq!(error.code, "session_work_dispatch_failed", "D1/E1");
    assert!(error.message.contains("injected failure"), "D1/E1");
    let persisted = repo.get("dispatch-admission").await.expect("D1/E2 intent");
    assert!(persisted.needs_work_dispatch(), "D1/E2 remains retryable");
    assert!(!persisted.is_terminal(), "D1/E2 is not falsely terminal");

    environments.recover_for("dispatch-admission");
    let recovered = application
        .recover_session_projection("dispatch-admission", Some("workspace"))
        .await
        .expect("D2/E3 queue recovered")
        .expect("D2/E3 Session remains present");
    assert!(
        recovered.session.needs_work_dispatch(),
        "D2/E3 awaits Worker ack"
    );
    assert_eq!(
        environments
            .dispatched
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        ["dispatch-admission".to_string()],
        "D2/E3 one stable dispatch identity"
    );
    application
        .admit_run_session("workspace", "dispatch-admission", "ignored")
        .await
        .expect("D2/E3 a driving event wakes the stable Work item");
    assert!(
        environments
            .awakened
            .lock()
            .unwrap()
            .contains("dispatch-admission"),
        "D2/E3 the admitted event explicitly wakes completed Work"
    );
    assert!(refresh.calls.load(Ordering::SeqCst) >= 3, "D3-D4");
}

#[tokio::test]
async fn worker_placed_cloud_session_admits_the_event_that_triggers_realization() {
    // Cause/effect decision table: C1 frozen placement=Worker; C2 Environment
    // is Cloud rather than self-hosted; C3 Session is Preparing; C4 no
    // late Session input. Effect E1 admission installs the dispatch projection
    // and succeeds without manufacturing Environment WorkQueue work;
    // the accepted event is what creates the Runtime Run claimed by a Worker.
    // Local Preparing is independently rejected by activity rule A8.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let mut session = persisted("cloud-worker-admission", false, "preparing");
    let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &mut session.baseline
    else {
        unreachable!("fixture is frozen")
    };
    baseline.runtime_placement = SessionRuntimePlacement::Worker;
    create(repo.as_ref(), session).await;
    let application = application(repo.clone(), environments.clone());

    application
        .admit_run_session("workspace", "cloud-worker-admission", "ignored")
        .await
        .expect("E1 Worker-owned realization is triggered by this admitted event");
    assert!(
        environments.dispatched.lock().unwrap().is_empty(),
        "E1 Cloud Environment does not enter the self-hosted WorkQueue"
    );
    assert_eq!(
        repo.get("cloud-worker-admission")
            .await
            .expect("E1 durable Session")
            .execution,
        SessionExecutionState::Preparing,
        "E1 only the claimed Worker may acknowledge physical realization"
    );
}

#[tokio::test]
async fn recovered_running_activity_admits_a_fenced_successor() {
    // Crash-recovery cause/effect graph: C1 a prior process committed Running
    // plus activity epoch 1; C2 no external Worker realization is required; C3
    // a successor driving event enters after restart. C1+C2+C3 => admission
    // succeeds and begin_activity can advance the epoch; Preparing/Activating/
    // Rescheduling and terminal partitions remain rejected by the shared
    // `admits_activity` table. FMECA: rejecting Running permanently strands the
    // Outcome/continuation; treating every nonterminal state as ready bypasses
    // realization convergence.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let mut running = persisted("crash-orphaned-activity", false, "running");
    running.activity_epoch = 1;
    create(repo.as_ref(), running).await;
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    application
        .admit_run_session("workspace", "crash-orphaned-activity", "agent")
        .await
        .expect("Running is an execution-ready, epoch-fenced state");
    let successor = application
        .begin_activity("crash-orphaned-activity")
        .await
        .expect("successor activity advances the durable fence");
    assert_eq!(successor.execution, SessionExecutionState::Running);
    assert_eq!(successor.activity_epoch, 2);
}

#[tokio::test]
async fn public_protocol_run_projects_durable_running_until_runtime_settles() {
    // Hosted MCP authorization cause/effect graph: C1 an AI SDK/AG-UI/A2A Run
    // has passed the canonical Session owner/Agent admission; C2 Runtime is
    // executing; C3 Runtime settles; C4 a foreign owner replays the exact stable
    // operation after its activity receipt exists. Effects: E1 the durable Session is Running
    // throughout C2 so an internal MCP `tools/call` can prove a live Run; E2 C3
    // closes the same activity and returns the Session to Idle; E3 C4 is rejected
    // before Runtime even though response-loss recovery truth exists. Constraint: the
    // protocol keeps its original Message/stream path; SessionApplication is the
    // sole activity owner. Decision table: P1=C1+C2=>E1; P2=C1+C3=>E2;
    // P3=C4=>E3. The
    // adjacent decorator test covers admission and Runtime failures.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        repository.as_ref(),
        persisted("protocol-running", false, "idle"),
    )
    .await;
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let session_runtime = Arc::new(BlockingSessionRunRuntime {
        entered: entered.clone(),
        release: release.clone(),
        application: std::sync::OnceLock::new(),
    });
    let application = Arc::new(application_with_runtime(
        session_runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    assert!(
        session_runtime
            .application
            .set(Arc::downgrade(&application))
            .is_ok(),
        "install test settlement observer"
    );
    let protocol = SessionRunApplication::new(
        Arc::new(ProtocolProjectionRuntime),
        application.clone(),
        |_| "workspace".into(),
        |_| Some("agent".into()),
    );

    let drive = tokio::spawn(async move {
        awaken_session_contract::RunApplication::run(
            &protocol,
            "protocol-operation",
            "protocol-running",
            None,
            vec![awaken_agent_contract::agent::message::Message::text(
                awaken_agent_contract::agent::message::Id("protocol-input".into()),
                awaken_agent_contract::agent::message::Role::User,
                "hello",
            )],
        )
        .await
    });
    entered
        .acquire()
        .await
        .expect("P1 Runtime entered")
        .forget();
    assert_eq!(
        repository
            .get("protocol-running")
            .await
            .expect("P1 durable Session")
            .execution,
        SessionExecutionState::Running,
        "P1/E1"
    );

    release.add_permits(1);
    drive.await.expect("P2 join").expect("P2 Run settled");
    assert_eq!(
        repository
            .get("protocol-running")
            .await
            .expect("P2 durable Session")
            .execution,
        SessionExecutionState::Idle,
        "P2/E2"
    );

    let foreign = SessionRunApplication::new(
        Arc::new(ProtocolProjectionRuntime),
        application,
        |_| "foreign-workspace".into(),
        |_| Some("agent".into()),
    );
    assert!(
        awaken_session_contract::RunApplication::run(
            &foreign,
            "protocol-operation",
            "protocol-running",
            None,
            vec![awaken_agent_contract::agent::message::Message::text(
                awaken_agent_contract::agent::message::Id("protocol-input".into()),
                awaken_agent_contract::agent::message::Role::User,
                "hello",
            )],
        )
        .await
        .is_err(),
        "P3/E3 a receipt never bypasses owner authorization"
    );
}

use super::*;

#[derive(Default)]
struct ColdEventRuntime {
    prepared: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionRuntime for ColdEventRuntime {
    async fn prepare_session(
        &self,
        _thread: &str,
        _init: awaken_session_contract::SessionInit,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.prepared
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_contract::StepOutcome, awaken_session_contract::RunError> {
        if !self.prepared.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(awaken_session_contract::RunError::internal(
                "Runtime executed before its frozen Session projection was installed",
            ));
        }
        Ok(awaken_session_contract::StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_contract::StepOutcome, awaken_session_contract::RunError> {
        unreachable!("cold event test never resumes")
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_contract::StepOutcome, awaken_session_contract::RunError> {
        unreachable!("cold event test never resumes a custom tool")
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<awaken_session_contract::OutcomeDrive, awaken_session_contract::RunError> {
        unreachable!("cold event test never defines an outcome")
    }

    fn model(&self) -> String {
        "cold-event-model".into()
    }
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
async fn managed_event_recovers_a_cold_worker_dispatch_projection_before_runtime() {
    // Cause/effect graph: C1 a Managed `/events` command enters the Session
    // application directly; C2 the Coordinator has restarted and therefore has
    // no process-local Runtime projection; C3 the frozen placement is Worker;
    // C4 the Cloud Environment needs no self-hosted WorkQueue item. Effects:
    // E1 canonical Run admission installs the exact frozen projection; E2 only
    // then may the activity epoch advance and Runtime execute; E3 this isolated
    // adapter test preserves Worker-owned Preparing (the real Worker owns its
    // realization acknowledgement) without inventing Environment work.
    //
    // | Rule | Managed event | Cold projection | Placement | Environment | Effect |
    // |---|---|---|---|---|---|
    // | E1 | yes | yes | Worker | Cloud | prepare -> run -> Preparing; no WorkQueue item |
    // | E2 | ordinary protocol | any | any | any | owned by AdmittedRunApplication |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let runtime = Arc::new(ColdEventRuntime::default());
    let mut session = persisted("cold-managed-event", false, "preparing");
    let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &mut session.baseline
    else {
        unreachable!("fixture is frozen")
    };
    baseline.runtime_placement = SessionRuntimePlacement::Worker;
    create(repo.as_ref(), session).await;
    let application = application_with_runtime(runtime.clone(), repo.clone(), environments.clone());

    let outcome = application
        .run_session_message(
            "agent",
            "cold-managed-event",
            vec![awaken_agent_contract::agent::content::ContentBlock::text(
                "continue after restart",
            )],
            None,
            Arc::new(DiscardProgress),
        )
        .await
        .expect("E1/E2 cold Managed event is admitted before Runtime");

    assert!(
        runtime.prepared.load(std::sync::atomic::Ordering::SeqCst),
        "E1 frozen projection installed"
    );
    assert_eq!(
        outcome.session.execution,
        SessionExecutionState::Preparing,
        "E3"
    );
    assert_eq!(outcome.session.activity_epoch, 1, "E2");
    assert!(environments.dispatched.lock().unwrap().is_empty(), "E3");
}

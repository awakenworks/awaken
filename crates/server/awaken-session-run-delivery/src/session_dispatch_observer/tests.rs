use super::*;
use awaken_agent_contract::agent::run::EndCause;
use awaken_session_execution_contract::{
    SessionAgentMessageCommand, SessionAgentMessageReceipt, SessionAgentRosterEntry,
    SessionThreadToolReplyCommand,
};
use awaken_session_types::RunError;
use std::sync::Mutex;

#[derive(Debug, PartialEq)]
enum Observed {
    Prepare(SessionId, String, RunId, SessionRunActivityAdmissionMode),
    PreparedAdmit(
        SessionId,
        RunId,
        SessionRunActivityAdmissionMode,
        Option<u64>,
        u64,
    ),
    Admit(SessionId, String, RunId, SessionRunActivityAdmissionMode),
    Before(SessionAgentBoundaryCommand),
    After(SessionAgentBoundaryCommand),
}

#[derive(Default)]
struct RecordingApplication {
    calls: Mutex<Vec<Observed>>,
    fail: Option<&'static str>,
}

impl RecordingApplication {
    fn record(&self, phase: &'static str, observed: Observed) -> Result<(), RunError> {
        self.calls.lock().unwrap().push(observed);
        if self.fail == Some(phase) {
            return Err(RunError::unavailable(format!("original-{phase}-failure")));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl SessionAgentCoordination for RecordingApplication {
    async fn prepare_prepared_session_run_activity(
        &self,
        session: &SessionId,
        agent: &str,
        run: &RunId,
        mode: SessionRunActivityAdmissionMode,
    ) -> Result<Option<awaken_session_types::SessionRevision>, RunError> {
        self.record(
            "prepare",
            Observed::Prepare(session.clone(), agent.into(), run.clone(), mode),
        )?;
        Ok(Some(awaken_session_types::SessionRevision(9)))
    }
    async fn admit_prepared_session_run_activity(
        &self,
        session: &SessionId,
        run: &RunId,
        mode: SessionRunActivityAdmissionMode,
        preparation: awaken_session_execution_contract::PreparedSessionRunActivity,
    ) -> Result<SessionRunActivityAdmission, RunError> {
        self.record(
            "prepared-admit",
            Observed::PreparedAdmit(
                session.clone(),
                run.clone(),
                mode,
                preparation.projected_revision.map(|revision| revision.0),
                preparation.admitted_after_cursor,
            ),
        )?;
        Ok(SessionRunActivityAdmission::Admitted {
            session_activity_epoch: 17,
        })
    }
    async fn admit_session_run_activity(
        &self,
        session: &SessionId,
        agent: &str,
        run: &RunId,
        mode: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, RunError> {
        self.record(
            "admit",
            Observed::Admit(session.clone(), agent.into(), run.clone(), mode),
        )?;
        Ok(SessionRunActivityAdmission::Admitted {
            session_activity_epoch: 17,
        })
    }
    async fn settle_session_agent_boundary(
        &self,
        command: SessionAgentBoundaryCommand,
    ) -> Result<(), RunError> {
        self.record("settle", Observed::Before(command))
    }
    async fn finalize_session_agent_boundary(
        &self,
        command: SessionAgentBoundaryCommand,
    ) -> Result<(), RunError> {
        self.record("finalize", Observed::After(command))
    }
    async fn admit_session_model_request(
        &self,
        _: &SessionId,
        _: &ThreadId,
        _: &RunId,
    ) -> Result<bool, RunError> {
        unreachable!("dispatch observation does not authorize models")
    }
    async fn list_session_agents(
        &self,
        _: &SessionId,
    ) -> Result<Vec<SessionAgentRosterEntry>, RunError> {
        unreachable!("dispatch observation does not list tools' roster")
    }
    async fn send_session_agent_message(
        &self,
        _: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, RunError> {
        unreachable!("dispatch observation does not execute Agent tools")
    }
    async fn interrupt_session_thread(&self, _: &SessionId, _: &ThreadId) -> Result<(), RunError> {
        unreachable!("dispatch observation does not control execution")
    }
    async fn reply_session_thread_tool(
        &self,
        _: SessionThreadToolReplyCommand,
    ) -> Result<(), RunError> {
        unreachable!("dispatch observation does not reply to tools")
    }
}

#[tokio::test]
async fn shared_session_observer_prepares_then_admits_only_guarded_prepared_inputs() {
    // K1 causes: prepared kind, exact affinity, prepared revision and original
    // guarded Thread cursor; K2 missing preparation; K3 ordinary kind. Effects
    // E1 K1 selects prepare then bounded prepared admission, never old generic
    // projection/admission; E2 K2 fails closed; E3 K3 retains its original path
    // (covered by the unchanged original observer table below). The Worker and
    // authenticated HTTP tests own the real lock/release boundary.
    let app = Arc::new(RecordingApplication::default());
    let observer = SessionDispatchObserver::new(app.clone());
    let request = dispatch("session", None).with_prepared_session_command_identity();
    let exact = claim();
    let mode = SessionRunActivityAdmissionMode::RecoverOrAdmit;
    let revision = observer
        .prepare_session_run_activity(&request, &exact, mode)
        .await
        .unwrap();
    assert_eq!(
        revision,
        Some(awaken_session_types::SessionRevision(9)),
        "K1 prepared outside guard"
    );
    assert!(
        observer
            .admit_session_run_activity(&request, &exact, mode, None)
            .await
            .is_err(),
        "K2 no unprepared fallback"
    );
    assert_eq!(
        observer
            .admit_session_run_activity(
                &request,
                &exact,
                mode,
                Some(
                    awaken_session_execution_contract::PreparedSessionRunActivity {
                        projected_revision: revision,
                        admitted_after_cursor: 23
                    }
                )
            )
            .await
            .unwrap(),
        SessionRunActivityAdmission::Admitted {
            session_activity_epoch: 17
        },
        "K1 bounded admission"
    );
    let calls = app.calls.lock().unwrap();
    assert!(
        matches!(
            &calls[..],
            [
                Observed::Prepare(..),
                Observed::PreparedAdmit(_, _, _, Some(9), 23)
            ]
        ),
        "K1 only prepared path"
    );
}

fn dispatch(thread: &str, epoch: Option<u64>) -> RunDispatch {
    let mut request = RunDispatch::new(awaken_runtime_contract::RunActivation::new(
        RunId("source-run".into()),
        ThreadId(thread.into()),
        crate::test_support::publication("frozen-agent"),
        Vec::new(),
    ))
    .for_session(ThreadId("session".into()));
    request.session_activity_epoch = epoch;
    request
}

fn claim() -> RunClaim {
    RunClaim {
        run_id: RunId("source-run".into()),
        owner: "original-worker".into(),
        epoch: 5,
    }
}

#[tokio::test]
async fn shared_session_observer_preserves_admission_order_and_owned_application() {
    // Causes: C1 exact self-affine reservation without an epoch; C2 repair
    // mode RecoverOnly/RecoverOrAdmit; C3 absent affinity, child, foreign Run or
    // already-receipted reservation; C4 exact committed boundary; C5 both
    // observer roles outlive the caller's application Arc.
    // A1=C1+C2 -> exactly one application call with original mode/identity.
    // A2=C3 -> Rejected and no call. B1=C4 -> Before then After carry the exact
    // same original boundary, never another activity receipt. L1=C5 -> retain
    // the one application; L2=both roles dropped -> release it. The recorder
    // observes a port only; Session/Dispatch persistence is not modeled here.
    let app = Arc::new(RecordingApplication::default());
    let weak = Arc::downgrade(&app);
    let observer = Arc::new(SessionDispatchObserver::new(app.clone()));
    let admission: Arc<dyn SessionRunAdmissionObserver> = observer.clone();
    let settlement: Arc<dyn DispatchSettlementObserver> = observer.clone();
    assert_eq!(
        Arc::as_ptr(&admission) as *const (),
        Arc::as_ptr(&settlement) as *const (),
        "one allocation, two existing roles"
    );
    assert!(
        !settlement.owns_dispatch_fence(),
        "the existing Worker owns the local fence"
    );
    let exact = claim();
    for mode in [
        SessionRunActivityAdmissionMode::RecoverOnly,
        SessionRunActivityAdmissionMode::RecoverOrAdmit,
    ] {
        assert_eq!(
            admission
                .admit_session_run_activity(&dispatch("session", None), &exact, mode, None)
                .await
                .unwrap(),
            SessionRunActivityAdmission::Admitted {
                session_activity_epoch: 17
            },
            "A1"
        );
        assert_eq!(
            app.calls.lock().unwrap().last(),
            Some(&Observed::Admit(
                "session".into(),
                "frozen-agent".into(),
                exact.run_id.clone(),
                mode
            )),
            "A1 exact facts"
        );
    }
    let before = app.calls.lock().unwrap().len();
    for case in 0..4 {
        let mut request = dispatch("session", None);
        let mut requested_claim = exact.clone();
        match case {
            0 => request.session_thread_id = None,
            1 => request.activation.thread_id = ThreadId("child".into()),
            2 => requested_claim.run_id = RunId("foreign".into()),
            _ => request.session_activity_epoch = Some(17),
        }
        assert_eq!(
            admission
                .admit_session_run_activity(
                    &request,
                    &requested_claim,
                    SessionRunActivityAdmissionMode::RecoverOrAdmit,
                    None
                )
                .await
                .unwrap(),
            SessionRunActivityAdmission::Rejected,
            "A2/{case}"
        );
    }
    assert_eq!(
        app.calls.lock().unwrap().len(),
        before,
        "A2 no dependency call"
    );
    let request = dispatch("child", Some(17));
    let state = RunState::Ended(EndCause::Cancelled);
    settlement
        .before_settle(&request, &exact, &state, true)
        .await
        .unwrap();
    settlement
        .after_settle(&request, &exact, &state, true)
        .await
        .unwrap();
    let expected = SessionAgentBoundaryCommand {
        session_id: "session".into(),
        source_thread_id: ThreadId("child".into()),
        source_run_id: exact.run_id.clone(),
        source_agent_id: "frozen-agent".into(),
        session_activity_epoch: 17,
        cancellation_requested: true,
    };
    assert_eq!(
        &app.calls.lock().unwrap()[before..],
        &[
            Observed::Before(expected.clone()),
            Observed::After(expected)
        ],
        "B1 exact order and facts"
    );
    drop(app);
    drop(observer);
    assert!(
        weak.upgrade().is_some(),
        "L1 retained by the two selected roles"
    );
    drop(admission);
    assert!(
        weak.upgrade().is_some(),
        "L1 settlement still retains the same owner"
    );
    drop(settlement);
    assert!(weak.upgrade().is_none(), "L2 final role releases it");
}

#[tokio::test]
async fn shared_session_observer_preserves_each_original_application_failure() {
    // F1/F2/F3: valid admission/before-settle/after-settle plus an application
    // failure -> the same public diagnostic, exactly one callback and no retry.
    // The adapter never converts a failure to a fabricated receipt or success.
    for phase in ["admit", "settle", "finalize"] {
        let app = Arc::new(RecordingApplication {
            fail: Some(phase),
            ..Default::default()
        });
        let observer = SessionDispatchObserver::new(app.clone());
        let exact = claim();
        let error = match phase {
            "admit" => observer
                .admit_session_run_activity(
                    &dispatch("session", None),
                    &exact,
                    SessionRunActivityAdmissionMode::RecoverOnly,
                    None,
                )
                .await
                .unwrap_err(),
            "settle" => observer
                .before_settle(
                    &dispatch("child", Some(17)),
                    &exact,
                    &RunState::Awaiting,
                    false,
                )
                .await
                .unwrap_err(),
            _ => observer
                .after_settle(
                    &dispatch("child", Some(17)),
                    &exact,
                    &RunState::Awaiting,
                    false,
                )
                .await
                .unwrap_err(),
        };
        let original = RunError::unavailable(format!("original-{phase}-failure")).to_string();
        assert_eq!(error.0, original, "{phase} exact source failure");
        assert_eq!(
            error.to_string(),
            format!("dispatch settlement observer failed: {original}"),
            "{phase} original public wrapper"
        );
        assert_eq!(
            app.calls.lock().unwrap().len(),
            1,
            "{phase} no hidden retry"
        );
    }
}

#[tokio::test]
async fn selected_session_observer_configures_the_shared_worker_without_an_executor() {
    use crate::SessionBoundaryEffects as _;
    use awaken_agent_contract::thread::commit::{RunDisposition, commit_run};
    use awaken_dispatch_contract::{DispatchClaims as _, DispatchEnqueue as _, DispatchReads as _};
    // C1 one real claimed dispatch and its committed child terminal; C2 the
    // selected application configures both observer roles; C3 original root
    // owners drop; C4 the same Worker recovers and settles the committed Run.
    // W1=C1+C2+C3 -> the Worker retains the selected application, not an SDK;
    // W2=W1+C4 -> original Before/After order and exact coordinates, row Done,
    // no model or Environment; W3=Worker dropped -> application released.
    let clock = awaken_time::system_clock();
    let store = Arc::new(awaken_dispatch::MemoryDispatchStore::new(clock.clone()));
    let request = dispatch("child", Some(17));
    store.enqueue(request.clone()).await.unwrap();
    let claimed = store
        .claim("original-worker", 30_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let commit = Arc::new(crate::SessionCommit::Local(Arc::new(
        awaken_dispatch_erasure::LocalCommitAdapter::projected(
            awaken_store_inmem::MemoryCommitCoordinator::new(clock.clone()),
        ),
    )));
    commit_run(
        commit.as_ref(),
        request.thread_id(),
        RunDisposition::ended(request.run_id().clone(), EndCause::NaturalEnd),
        Vec::new(),
        Vec::new(),
    )
    .await
    .unwrap();
    let app = Arc::new(RecordingApplication::default());
    let weak = Arc::downgrade(&app);
    let observer = Arc::new(SessionDispatchObserver::new(app.clone()));
    let worker = crate::session_dispatch_worker(
        Arc::new(awaken_runtime::Runtime::new().with_clock(clock)),
        awaken_dispatch_erasure::DispatchBinding::local(Arc::new(
            awaken_dispatch_erasure::AnyCoordinatorDispatch::from_durable_dispatch(store.clone()),
        )),
        commit.clone(),
        "original-worker",
    )
    .unwrap();
    let worker = observer
        .clone()
        .configure(worker, &claimed, commit, None)
        .await
        .unwrap();
    drop(app);
    drop(observer);
    assert!(
        weak.upgrade().is_some(),
        "W1 selected roles retain the original application"
    );
    assert_eq!(
        worker.settle_claimed_terminal(claimed).await.unwrap(),
        Some((
            request.run_id().clone(),
            RunState::Ended(EndCause::NaturalEnd)
        )),
        "W2 original terminal"
    );
    {
        let retained = weak.upgrade().unwrap();
        let calls = retained.calls.lock().unwrap();
        assert!(
            matches!(calls.as_slice(), [Observed::Before(_), Observed::After(_)]),
            "W2 original callback order"
        );
        for command in calls.iter() {
            let (Observed::Before(command) | Observed::After(command)) = command else {
                panic!("W2 no fresh activity");
            };
            assert_eq!(command.session_id, "session", "W2 physical Session");
            assert_eq!(
                command.source_thread_id,
                *request.thread_id(),
                "W2 logical child"
            );
            assert_eq!(command.source_run_id, *request.run_id(), "W2 original Run");
            assert_eq!(command.session_activity_epoch, 17, "W2 original activity");
        }
    }
    assert!(
        store.list_dispatches().await.unwrap().is_empty(),
        "W2 original Dispatch settled"
    );
    drop(worker);
    assert!(weak.upgrade().is_none(), "W3 selected application released");
}

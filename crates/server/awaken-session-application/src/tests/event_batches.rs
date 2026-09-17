use awaken_session_contract::SessionId;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::AtomicU64;

use awaken_agent_contract::agent::{
    content::ContentBlock,
    message::{Id as MessageId, Message, Role},
    run::{EndCause, Id as RunId, RunState},
    thread::Id as ThreadId,
};
use awaken_session_contract::{
    AdmitSessionRun, AdmittedSessionRun, CoordinatedThreadLink, CoordinatedThreadTarget,
    OutcomeDrive, OutcomeReport, SessionEventCommand, SessionEventInput, SessionEventInterrupt,
    SessionEventToolReply, SessionEventToolReplyKind, SessionInitialEventPlan,
    SessionOutcomeRubric, SessionRunActivation, SessionRunDelivery, SessionRunReservation,
    SessionRuntime, SessionThreadTarget, SessionThreadToolReplyCommand,
    SessionThreadToolReplyDelivery, SessionThreadToolReplyFence, SessionUserRunCommand,
    StepOutcome, ToolPermissionDecision,
};

use super::*;

#[derive(Default)]
struct EventBatchRuntime {
    admission_barrier: Mutex<Option<Arc<tokio::sync::Barrier>>>,
    reservation_barriers: Mutex<Option<(Arc<tokio::sync::Barrier>, Arc<tokio::sync::Barrier>)>>,
    reserved: Mutex<BTreeMap<String, AdmitSessionRun>>,
    projected_resources: Mutex<Option<(u64, awaken_session_contract::ResolvedSessionResources)>>,
    reserved_resource_generations:
        Mutex<BTreeMap<String, (u64, awaken_session_contract::ResolvedSessionResources)>>,
    reservation_outcomes: Mutex<VecDeque<SessionRunReservation>>,
    fail_reservation: AtomicBool,
    fail_activation_once: AtomicBool,
    activated: Mutex<BTreeMap<String, u64>>,
    states: Mutex<HashMap<String, RunState>>,
    run_state_reads: AtomicUsize,
    latest_run: Mutex<Option<RunId>>,
    committed: Mutex<Vec<Message>>,
    recovery_projection_override: Mutex<Option<(Vec<u64>, u64)>>,
    trace: Mutex<Vec<String>>,
    outcomes: Mutex<BTreeMap<String, (String, String, u32)>>,
    outcome_commit_cursors: Mutex<BTreeMap<String, u64>>,
    next_outcome_commit_cursor: AtomicU64,
    outcome_prepare_calls: Mutex<Vec<String>>,
    outcome_busy: AtomicBool,
    outcome_active: AtomicBool,
    outcome_continue_calls: AtomicUsize,
    dispatch_projection_installs: Mutex<Vec<String>>,
    outcome_completed: AtomicBool,
    tool_reply_prior_epoch: AtomicU64,
    tool_reply_calls: AtomicUsize,
    staged_tool_replies: Mutex<BTreeMap<String, SessionThreadToolReplyCommand>>,
    fail_tool_reply_after_stage_once: AtomicBool,
    primary_interrupt_calls: AtomicUsize,
    primary_interrupt_effects: Mutex<BTreeSet<String>>,
    child_interrupt_calls: AtomicUsize,
    child_interrupt_effects: Mutex<BTreeSet<(String, String)>>,
    fail_child_interrupt_once: AtomicBool,
    fail_primary_interrupt_once: AtomicBool,
}

impl EventBatchRuntime {
    fn end(&self, run_id: &RunId) {
        self.states
            .lock()
            .unwrap()
            .insert(run_id.0.clone(), RunState::Ended(EndCause::NaturalEnd));
    }

    fn await_run(&self, run_id: &RunId) {
        self.states
            .lock()
            .unwrap()
            .insert(run_id.0.clone(), RunState::Awaiting);
    }

    fn activity_epoch(&self, run_id: &RunId) -> u64 {
        *self
            .activated
            .lock()
            .unwrap()
            .get(&run_id.0)
            .expect("Run was activated")
    }

    fn commit_staged_system_messages(&self) {
        let staged = self.staged_tool_replies.lock().unwrap();
        let mut committed = self.committed.lock().unwrap();
        for command in staged.values() {
            let Some(system) = &command.accompanying_system else {
                continue;
            };
            let message = Message::new(
                MessageId::session_system(command.session_id.as_str(), &system.operation_id),
                Role::System,
                system.content.clone(),
            );
            if !committed.iter().any(|existing| existing.id == message.id) {
                committed.push(message);
            }
        }
    }

    fn committed_message_projection(&self) -> (Vec<Message>, Vec<u64>, u64) {
        let messages = self.committed.lock().unwrap().clone();
        if let Some((cursors, store_cursor)) =
            self.recovery_projection_override.lock().unwrap().clone()
        {
            return (messages, cursors, store_cursor);
        }
        let message_commit_cursors = (0..messages.len())
            .map(|index| index as u64 + 1)
            .collect::<Vec<_>>();
        let store_cursor = message_commit_cursors.last().copied().unwrap_or(1);
        (messages, message_commit_cursors, store_cursor)
    }
}

#[async_trait::async_trait]
impl SessionRuntime for EventBatchRuntime {
    fn observe_realization_request(
        &self,
    ) -> Result<awaken_runtime_contract::authority_lease::AuthorityProofStarted, RunError> {
        crate::tests::test_realization_request_started()
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        if mode.prepares_session() {
            self.trace
                .lock()
                .unwrap()
                .push(format!("project:{thread}:{}", projection.resource_revision));
            *self.projected_resources.lock().unwrap() =
                Some((projection.resource_revision, projection.resources.clone()));
        }
        if matches!(
            mode,
            awaken_session_contract::SessionProjectionInstallMode::Dispatch
        ) {
            self.dispatch_projection_installs
                .lock()
                .unwrap()
                .push(thread.to_string());
        }
        install_complete_test_projection(self, thread, projection, mode).await
    }

    async fn reserve_session_run(
        &self,
        command: AdmitSessionRun,
    ) -> Result<SessionRunReservation, RunError> {
        let barriers = self.reservation_barriers.lock().unwrap().take();
        if let Some((entered, release)) = barriers {
            entered.wait().await;
            release.wait().await;
        }
        self.trace
            .lock()
            .unwrap()
            .push(format!("reserve:{}", command.run_id.0));
        if self.fail_reservation.load(Ordering::SeqCst) {
            return Err(RunError::bad_request("scripted reservation rejection"));
        }
        if let Some(outcome) = self.reservation_outcomes.lock().unwrap().pop_front() {
            return Ok(outcome);
        }
        if let Some(epoch) = self
            .activated
            .lock()
            .unwrap()
            .get(&command.run_id.0)
            .copied()
        {
            return Ok(SessionRunReservation::AlreadyActivated {
                session_activity_epoch: epoch,
            });
        }
        let mut reserved = self.reserved.lock().unwrap();
        match reserved.get(&command.run_id.0) {
            Some(existing) if existing == &command => Ok(SessionRunReservation::AlreadyReserved),
            Some(_) => Err(RunError::bad_request(
                "scripted Run id was reused with different input",
            )),
            None => {
                if let Some(generation) = self.projected_resources.lock().unwrap().clone() {
                    self.reserved_resource_generations
                        .lock()
                        .unwrap()
                        .insert(command.run_id.0.clone(), generation);
                }
                reserved.insert(command.run_id.0.clone(), command);
                Ok(SessionRunReservation::Reserved)
            }
        }
    }

    async fn activate_session_run(
        &self,
        delivery: SessionRunDelivery,
    ) -> Result<SessionRunActivation, RunError> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("activate:{}", delivery.run_id.0));
        if self.fail_activation_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::unavailable("scripted activation interruption"));
        }
        let prior = self
            .activated
            .lock()
            .unwrap()
            .insert(delivery.run_id.0.clone(), delivery.session_activity_epoch);
        if let Some(session_activity_epoch) = prior {
            return Ok(SessionRunActivation::AlreadyActivated {
                session_activity_epoch,
            });
        }
        self.states
            .lock()
            .unwrap()
            .insert(delivery.run_id.0.clone(), RunState::Running);
        *self.latest_run.lock().unwrap() = Some(delivery.run_id.clone());
        let command = self
            .reserved
            .lock()
            .unwrap()
            .get(&delivery.run_id.0)
            .cloned()
            .ok_or_else(|| RunError::internal("activated Run has no reservation"))?;
        let input = command.messages;
        let mut committed = self.committed.lock().unwrap();
        for message in input {
            if !committed.iter().any(|existing| existing.id == message.id) {
                committed.push(message);
            }
        }
        Ok(SessionRunActivation::Activated)
    }

    async fn session_run_state(
        &self,
        _session_id: &SessionId,
        run_id: &RunId,
    ) -> Result<Option<RunState>, RunError> {
        self.run_state_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.states.lock().unwrap().get(&run_id.0).cloned())
    }

    async fn committed_messages(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(self.committed.lock().unwrap().clone())
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        let barrier = self.admission_barrier.lock().unwrap().take();
        if let Some(barrier) = barrier {
            barrier.wait().await;
        }
        let staged_run_id = self
            .staged_tool_replies
            .lock()
            .unwrap()
            .values()
            .find(|command| command.target.thread_id(session_id.as_str()).0 == thread_id)
            .map(|command| command.expected_run_id.clone());
        if let Some(run_id) = staged_run_id {
            let thread_id = ThreadId(thread_id.to_string());
            let (messages, message_commit_cursors, store_cursor) =
                self.committed_message_projection();
            return Ok(Some(
                awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
                    commit_timestamps: Default::default(),
                    thread_id: thread_id.clone(),
                    claimed_run_id: run_id.clone(),
                    runs: vec![awaken_agent_contract::agent::run::Record {
                        id: run_id.clone(),
                        thread_id,
                        state: RunState::Awaiting,
                    }],
                    latest_run_id: Some(run_id),
                    messages,
                    message_commit_cursors,
                    state: Vec::new(),
                    state_commit_cursors: Vec::new(),
                    events: Vec::new(),
                    resume_tickets: Vec::new(),
                    thread_version: 1,
                    store_cursor,
                    next_commit_ordinal: 1,
                },
            ));
        }
        let Some(latest_run_id) = self.latest_run.lock().unwrap().clone() else {
            return Ok(None);
        };
        let thread_id = awaken_agent_contract::agent::thread::Id(thread_id.to_string());
        let runs = self
            .states
            .lock()
            .unwrap()
            .iter()
            .map(
                |(run_id, state)| awaken_agent_contract::agent::run::Record {
                    id: RunId(run_id.clone()),
                    thread_id: thread_id.clone(),
                    state: state.clone(),
                },
            )
            .collect();
        let (messages, message_commit_cursors, store_cursor) = self.committed_message_projection();
        Ok(Some(
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
                commit_timestamps: Default::default(),
                thread_id,
                claimed_run_id: latest_run_id.clone(),
                runs,
                latest_run_id: Some(latest_run_id),
                messages,
                message_commit_cursors,
                state: Vec::new(),
                state_commit_cursors: Vec::new(),
                events: Vec::new(),
                resume_tickets: Vec::new(),
                thread_version: 1,
                store_cursor,
                next_commit_ordinal: 1,
            },
        ))
    }

    async fn coordinated_threads(
        &self,
        session_id: &SessionId,
        _observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<CoordinatedThreadLink>, RunError> {
        Ok(vec![CoordinatedThreadLink {
            session_id: session_id.clone(),
            thread_id: ThreadId("event-child".into()),
            target: CoordinatedThreadTarget::Agent {
                agent_id: "event-child-agent".into(),
            },
            created_by_operation_id: "event-child-create".into(),
            latest_run_id: Some(RunId("event-child-run".into())),
        }])
    }

    async fn session_thread_tool_reply_fence(
        &self,
        command: &SessionThreadToolReplyCommand,
    ) -> Result<SessionThreadToolReplyFence, RunError> {
        if command.expected_run_id.0.trim().is_empty()
            || command.expected_correlation_id.trim().is_empty()
            || command.tool_use_id.trim().is_empty()
        {
            return Err(RunError::bad_request("incomplete scripted reply"));
        }
        let operation = command.activity_operation_id();
        let staged = self.staged_tool_replies.lock().unwrap();
        if staged.get(&operation) == Some(command) {
            return Ok(SessionThreadToolReplyFence {
                activity: awaken_session_contract::SessionThreadToolReplyActivity::Session {
                    prior_epoch: None,
                },
                already_applied: true,
            });
        }
        if staged.values().any(|existing| {
            existing.session_id == command.session_id
                && existing.expected_run_id == command.expected_run_id
                && existing.expected_correlation_id == command.expected_correlation_id
        }) {
            return Err(RunError::bad_request(
                "scripted awaiting correlation was answered by another operation",
            ));
        }
        drop(staged);
        let prior_session_activity_epoch = self.tool_reply_prior_epoch.load(Ordering::SeqCst);
        if prior_session_activity_epoch == 0 {
            return Err(RunError::internal(
                "scripted reply has no prior Session activity epoch",
            ));
        }
        Ok(SessionThreadToolReplyFence {
            activity: awaken_session_contract::SessionThreadToolReplyActivity::Session {
                prior_epoch: Some(prior_session_activity_epoch),
            },
            already_applied: false,
        })
    }

    async fn reply_session_thread_tool(
        &self,
        delivery: SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        self.tool_reply_calls.fetch_add(1, Ordering::SeqCst);
        self.trace
            .lock()
            .unwrap()
            .push(format!("reply:{}", delivery.command.tool_use_id));
        let operation = delivery.command.activity_operation_id();
        let mut staged = self.staged_tool_replies.lock().unwrap();
        if let Some(existing) = staged.get(&operation) {
            if existing != &delivery.command {
                return Err(RunError::bad_request(
                    "scripted reply operation changed payload",
                ));
            }
        } else {
            staged.insert(operation, delivery.command);
        }
        drop(staged);
        if self
            .fail_tool_reply_after_stage_once
            .swap(false, Ordering::SeqCst)
        {
            return Err(RunError::unavailable(
                "scripted crash after durable reply stage",
            ));
        }
        Ok(())
    }

    async fn interrupt_session_thread(
        &self,
        session_id: &SessionId,
        child_thread_id: &ThreadId,
    ) -> Result<(), RunError> {
        self.child_interrupt_calls.fetch_add(1, Ordering::SeqCst);
        self.trace
            .lock()
            .unwrap()
            .push(format!("interrupt:{}", child_thread_id.0));
        if self.fail_child_interrupt_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::unavailable(
                "scripted interruption before child effect",
            ));
        }
        self.child_interrupt_effects
            .lock()
            .unwrap()
            .insert((session_id.to_string(), child_thread_id.0.clone()));
        Ok(())
    }

    async fn prepare_outcome(
        &self,
        _thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, RunError> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("outcome:{outcome_id}"));
        self.outcome_prepare_calls
            .lock()
            .unwrap()
            .push(outcome_id.to_string());
        if self.outcome_busy.load(Ordering::SeqCst) {
            return Err(RunError::unavailable_classified(
                awaken_session_contract::OUTCOME_BUSY_CODE,
                "another Outcome is active",
            ));
        }
        let definition = (description.to_string(), rubric.to_string(), max_iterations);
        let mut outcomes = self.outcomes.lock().unwrap();
        let source_commit_cursor = if let Some(existing) = outcomes.get(outcome_id) {
            if existing != &definition {
                return Err(RunError::bad_request(
                    "Outcome id was reused with another definition",
                ));
            }
            *self
                .outcome_commit_cursors
                .lock()
                .unwrap()
                .get(outcome_id)
                .expect("fixture Outcome cursor exists with its definition")
        } else {
            outcomes.insert(outcome_id.to_string(), definition);
            let source_commit_cursor = self
                .next_outcome_commit_cursor
                .fetch_add(1, Ordering::SeqCst)
                .checked_add(1)
                .expect("fixture Outcome cursor exhausted");
            self.outcome_commit_cursors
                .lock()
                .unwrap()
                .insert(outcome_id.to_string(), source_commit_cursor);
            source_commit_cursor
        };
        self.outcome_active.store(true, Ordering::SeqCst);
        Ok(source_commit_cursor)
    }

    async fn continue_outcome(&self, _thread: &str) -> Result<Option<OutcomeDrive>, RunError> {
        self.outcome_continue_calls.fetch_add(1, Ordering::SeqCst);
        if !self.outcome_active.load(Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(if self.outcome_completed.load(Ordering::SeqCst) {
            self.outcome_active.store(false, Ordering::SeqCst);
            Some(OutcomeDrive::Completed(OutcomeReport {
                iterations: Vec::new(),
            }))
        } else {
            Some(OutcomeDrive::Awaiting)
        })
    }

    async fn run(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        NoopRuntime.run(agent, thread, content).await
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        NoopRuntime.resume(thread, tool_use_id, decision).await
    }

    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: Vec<ContentBlock>,
        is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        NoopRuntime
            .resume_custom(thread, tool_use_id, content, is_error)
            .await
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.primary_interrupt_calls.fetch_add(1, Ordering::SeqCst);
        self.trace.lock().unwrap().push("interrupt:primary".into());
        if self
            .fail_primary_interrupt_once
            .swap(false, Ordering::SeqCst)
        {
            return Err(RunError::unavailable(
                "scripted interruption before primary effect",
            ));
        }
        self.primary_interrupt_effects
            .lock()
            .unwrap()
            .insert(thread.to_string());
        Ok(())
    }

    fn model(&self) -> String {
        "event-batch-test-model".into()
    }
}

fn planned_session(session_id: &str, inputs: Vec<SessionEventInput>) -> PersistedSession {
    let mut session = persisted(session_id, false, "idle");
    session
        .install_initial_event_plan(
            SessionInitialEventPlan::compile(session_id, format!("initial:{session_id}"), inputs)
                .expect("valid initial Event plan"),
        )
        .expect("install initial Event plan");
    // The focused reconciler fixture starts after the ordinary realization
    // acknowledgement has promoted the root-owned batch activity.
    session.execution = awaken_session_contract::SessionExecutionState::Running;
    session
}

#[tokio::test]
async fn preferred_user_drive_accelerates_only_the_canonical_fifo_head() {
    // Cause/effect graph: C1 a just-accepted User batch is the ordinary global
    // FIFO head; C2 an earlier User batch remains unprocessed; C3 the caller
    // prefers the later batch. E1 C1 reserves and activates that exact Run
    // immediately through the canonical Event driver; E2 C2+C3 performs no
    // Run admission and leaves both durable commands queued for the lifecycle
    // supervisor. The existing receipt/Interrupt tests own their higher-priority
    // crossing rules, and the full Event recovery suite owns crash/replay.
    //
    // | Rule | Earlier User pending | Preferred batch is FIFO head | Effect |
    // |---|---|---|---|
    // | F1 | no | yes | reserve and activate preferred User (E1) |
    // | F2 | yes | no | no admission or activation (E2) |
    for earlier_pending in [false, true] {
        let session_id = if earlier_pending {
            "preferred-user-behind"
        } else {
            "preferred-user-head"
        };
        let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let mut session = planned_session(
            session_id,
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("first")],
            }],
        );
        if !earlier_pending {
            session.event_batches[0].events[0].processed = true;
            session.event_batches[0].events[0].projection_anchor =
                Some(awaken_session_contract::SessionEventProjectionAnchor {
                    source_commit_cursor: 1,
                });
        }
        create(repository.as_ref(), session).await;
        let runtime = Arc::new(EventBatchRuntime::default());
        let app = application_with_runtime(
            runtime.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let (_preferred, drive_error) = app
            .append_and_drive_session_event_batch_idempotent(
                &SessionId::from(session_id),
                vec![SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("preferred")],
                }],
                None,
                None,
                Some(("preferred-user".into(), "preferred-user-fingerprint".into())),
            )
            .await
            .expect("append preferred User batch");
        assert!(drive_error.is_none(), "F1-F2 request-local drive");
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            app.lifecycle_wakeup.notified(),
        )
        .await
        .expect("F1-F2 full recovery remains notified after the local attempt");
        assert_eq!(
            runtime.reserved.lock().unwrap().len(),
            usize::from(!earlier_pending),
            "F1-F2 reservation"
        );
        assert_eq!(
            runtime.activated.lock().unwrap().len(),
            usize::from(!earlier_pending),
            "F1-F2 activation"
        );
        let durable = repository.get(&SessionId::from(session_id)).await.unwrap();
        assert_eq!(
            durable.event_batches[0].events[0].processed, !earlier_pending,
            "F1 preserves completed predecessor; F2 preserves queued predecessor"
        );
        assert!(
            !durable.event_batches[1].events[0].processed,
            "F1-F2 activation is pending until committed Runtime settlement"
        );
    }
}

#[tokio::test]
async fn preferred_user_drive_failure_retains_receipt_and_recovery_wake() {
    // Causes: C1 the root CAS accepts one FIFO-head User command; C2 its
    // request-local activation fails before an effect. Effects: E1 return the
    // exact accepted batch and the retryable drive error; E2 retain the command
    // unprocessed; E3 notify the sole lifecycle supervisor after the failed
    // attempt. This is the failure row excluded from F1 above. Periodic recovery
    // remains the crash-between-accept-and-drive fallback and owns no second
    // receipt or execution path.
    let session_id = "preferred-user-drive-failure";
    let repository: Arc<dyn ManagedSessionRepository> =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(repository.as_ref(), persisted(session_id, false, "idle")).await;
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.fail_activation_once.store(true, Ordering::SeqCst);
    let app = application_with_runtime(
        runtime,
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let (accepted, drive_error) = app
        .append_and_drive_session_event_batch_idempotent(
            &SessionId::from(session_id),
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("retry through recovery")],
            }],
            None,
            None,
            Some(("failure-key".into(), "failure-fingerprint".into())),
        )
        .await
        .expect("E1 accepted receipt survives local drive failure");
    assert!(drive_error.is_some(), "E1 exact drive failure is reported");
    assert_eq!(accepted.events.len(), 1, "E1 exact receipt");
    assert!(
        !repository.get(&SessionId::from(session_id)).await.unwrap().event_batches[0].events[0].processed,
        "E2"
    );
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        app.lifecycle_wakeup.notified(),
    )
    .await
    .expect("E3");
}

async fn finalize_applied_user_dispatch(
    app: &SessionApplication,
    runtime: &EventBatchRuntime,
    session_id: &str,
    run_id: &RunId,
) {
    // Cause/effect boundary fixture: C1 the test Runtime has committed an
    // Awaiting/Ended User Run and its simulated Dispatch CAS is Applied; E1 the
    // production post-settlement callback atomically records the receipt,
    // removes that Run's exact activity and publishes processed. Tests that do
    // not call this helper intentionally model the pre-Dispatch window.
    let activity_epoch = runtime.activity_epoch(run_id);
    let boundary = awaken_session_contract::session_agent_run_boundary(
        &runtime.states.lock().unwrap()[&run_id.0],
    )
    .unwrap();
    app.settle_user_event_dispatch_observed(
        &SessionId::from(session_id),
        activity_epoch,
        awaken_session_contract::SessionRuntimeIntervalObservation {
            activity_epoch,
            thread_id: ThreadId(session_id.into()),
            run_id: run_id.clone(),
            lifecycle_cursor: awaken_agent_contract::RunLifecycleCursor(activity_epoch),
            source_commit_cursor: 1,
        },
        run_id,
        boundary,
    )
    .await
    .expect("simulated applied Dispatch finalizes the User Event");
}

fn tool_reply_input(tool_use_id: &str) -> SessionEventInput {
    tool_reply_input_for_target(tool_use_id, SessionThreadTarget::Primary)
}

fn tool_reply_input_for_target(
    tool_use_id: &str,
    target: SessionThreadTarget,
) -> SessionEventInput {
    SessionEventInput::ToolReply(SessionEventToolReply {
        tool_request_event_id: format!("evt_{tool_use_id}"),
        target,
        expected_run_id: RunId("awaiting-run".into()),
        expected_correlation_id: "awaiting-correlation".into(),
        expected_thread_version: None,
        answered_pending_commit_cursor: Some(777),
        runtime_tool_use_id: tool_use_id.into(),
        reply: SessionEventToolReplyKind::ToolResult {
            content: Some(vec![ContentBlock::text("tool result")]),
            is_error: false,
        },
    })
}

fn tool_reply_activity_operation_id(
    session_id: &str,
    batch: &awaken_session_contract::SessionEventBatch,
) -> String {
    let ordinal = batch
        .events
        .iter()
        .position(|entry| matches!(&entry.event, SessionEventCommand::ToolReply { .. }))
        .expect("fixture batch contains a ToolReply");
    let reply = match &batch.events[ordinal].event {
        SessionEventCommand::ToolReply { reply, .. } => reply,
        _ => unreachable!("ToolReply ordinal was selected above"),
    };
    let accompanying_system = batch.events.get(ordinal + 1).and_then(|entry| {
        if let SessionEventCommand::SystemMessage {
            operation_id,
            content,
        } = &entry.event
        {
            Some(awaken_session_contract::SessionUserRunSystemInput {
                operation_id: operation_id.clone(),
                content: content.clone(),
            })
        } else {
            None
        }
    });
    reply
        .delivery_command(session_id, accompanying_system)
        .activity_operation_id()
}

fn running_session_with_activity(session_id: &str, epoch: u64) -> PersistedSession {
    let mut session = persisted(session_id, false, "running");
    session.activity_epoch = epoch;
    session.active_activity_epochs.insert(epoch);
    session
}

#[tokio::test]
async fn model_admission_wakes_unanchored_primary_input_without_releasing_fifo() {
    // Causes: C1 the current User input is absent/present; C2 model admission
    // wakes the root; C3 the Run is Running/terminal; C4 Dispatch-Applied has
    // not settled the command. Effects: E1 only present input gains its anchor;
    // E2 neither state publishes processed or activates queued work; E3 the
    // terminal pre-Dispatch scan observes state once and remains Pending.
    // Decision rules: M1 absent+Running=>E2; M2 present+Running=>E1+E2;
    // M3 present+terminal+C4=>E2+E3. Dispatch settlement is the adjacent
    // `finalize_applied_user_dispatch` path and remains the sole success row.
    for has_input in [false, true] {
        let session_id = if has_input {
            "running-anchored"
        } else {
            "running-no-input"
        };
        let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let session = planned_session(
            session_id,
            vec![
                SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("first")],
                },
                SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("queued")],
                },
            ],
        );
        let (run_id, operation_id) = match &session.event_batches[0].events[0].event {
            SessionEventCommand::UserMessage {
                run_id,
                operation_id,
                ..
            } => (run_id.clone(), operation_id.clone()),
            _ => unreachable!(),
        };
        create(repository.as_ref(), session).await;
        let runtime = Arc::new(EventBatchRuntime::default());
        runtime
            .states
            .lock()
            .unwrap()
            .insert(run_id.0.clone(), RunState::Running);
        *runtime.latest_run.lock().unwrap() = Some(run_id.clone());
        if has_input {
            runtime.committed.lock().unwrap().push(Message::new(
                MessageId::session_event_input(session_id, &operation_id),
                Role::User,
                vec![ContentBlock::text("first")],
            ));
        }
        let app = application_with_runtime(
            runtime.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        // The actual model gate, not a manually scheduled recovery interval,
        // must wake publication for this exact pending primary input.
        assert!(
            awaken_session_contract::SessionAgentCoordination::admit_session_model_request(
                &app,
                &SessionId::from(session_id),
                &ThreadId(session_id.into()),
                &RunId("not-the-current-run".into()),
            )
            .await
            .is_err()
        );
        awaken_session_contract::SessionAgentCoordination::admit_session_model_request(
            &app,
            &SessionId::from(session_id),
            &ThreadId("event-child".into()),
            &run_id,
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                app.lifecycle_wakeup.notified(),
            )
            .await
            .is_err(),
            "neither an invalid Run nor a child request may wake the primary input"
        );
        awaken_session_contract::SessionAgentCoordination::admit_session_model_request(
            &app,
            &SessionId::from(session_id),
            &ThreadId(session_id.into()),
            &run_id,
        )
        .await
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            app.lifecycle_wakeup.notified(),
        )
        .await
        .expect("model admission must wake the existing supervisor before completion");
        app.drive_session_event_batches(&SessionId::from(session_id), None)
            .await
            .unwrap();
        let first = repository.get(&SessionId::from(session_id)).await.unwrap();
        let entries = &first.event_batches[0].events;
        assert_eq!(
            entries[0].projection_anchor.map(|a| a.source_commit_cursor),
            has_input.then_some(1)
        );
        assert!(entries.iter().all(|entry| !entry.processed));
        assert!(entries[1].projection_anchor.is_none());
        assert!(runtime.activated.lock().unwrap().is_empty());
        app.drive_session_event_batches(&SessionId::from(session_id), None)
            .await
            .unwrap();
        let replay = repository.get(&SessionId::from(session_id)).await.unwrap();
        assert_eq!(
            first.revision, replay.revision,
            "anchor replay must not mutate the root"
        );
        if has_input {
            awaken_session_contract::SessionAgentCoordination::admit_session_model_request(
                &app,
                &SessionId::from(session_id),
                &ThreadId(session_id.into()),
                &run_id,
            )
            .await
            .unwrap();
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(10),
                    app.lifecycle_wakeup.notified(),
                )
                .await
                .is_err(),
                "subsequent requests must not wake recovery for an already anchored input"
            );
            runtime.end(&run_id);
            let reads_before_terminal_drive = runtime.run_state_reads.load(Ordering::SeqCst);
            app.drive_session_event_batches(&SessionId::from(session_id), None)
                .await
                .unwrap();
            assert_eq!(
                runtime.run_state_reads.load(Ordering::SeqCst) - reads_before_terminal_drive,
                1,
                "terminal truth before Dispatch settlement remains Pending without reselecting the same command"
            );
            let ended = repository.get(&SessionId::from(session_id)).await.unwrap();
            assert!(
                !ended.event_batches[0].events[0].processed,
                "terminal truth without an admitted activity/Dispatch receipt stays pending"
            );
            assert_eq!(
                ended.event_batches[0].events[0].projection_anchor,
                entries[0].projection_anchor
            );
            assert_eq!(
                runtime.activated.lock().unwrap().len(),
                0,
                "terminal Run truth without Dispatch settlement cannot release FIFO"
            );
        }
    }
}

#[tokio::test]
async fn unreplayed_user_run_anchors_its_receipt_without_inventing_a_message() {
    // Cause/effect graph: C1 accepted User command owns exact Run R; C2 R
    // commits Cancelled before input, or Indeterminate after opaque ACP claim
    // recovery (ADR-0073); C3 another terminal cause lacks the input Message.
    // E1 every terminal class remains pending without the applied Dispatch
    // receipt; E2 never invent a Message or replay a prompt; E3 preserve R's
    // end cause, including unknown outcome. U1-U6 each select E1-E3. This
    // freezes the strengthened boundary: Thread terminality is commit evidence,
    // not proof that Dispatch and Session activity settlement completed.
    for (session_id, end) in [
        ("cancelled-user-receipt", EndCause::Cancelled),
        ("indeterminate-user-receipt", EndCause::Indeterminate),
        ("natural-user-receipt", EndCause::NaturalEnd),
        ("maxsteps-user-receipt", EndCause::MaxSteps),
        ("stopped-user-receipt", EndCause::Stopped("budget".into())),
        (
            "error-user-receipt",
            EndCause::Error(awaken_agent_contract::agent::run::Failure::CapabilityBound),
        ),
    ] {
        let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("U1-U3 repository"),
        );
        let session = planned_session(
            session_id,
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("accepted input")],
            }],
        );
        let run_id = match &session.event_batches[0].events[0].event {
            SessionEventCommand::UserMessage { run_id, .. } => run_id.clone(),
            _ => unreachable!("fixture compiles one User command"),
        };
        create(repository.as_ref(), session).await;
        let runtime = Arc::new(EventBatchRuntime::default());
        runtime
            .states
            .lock()
            .unwrap()
            .insert(run_id.0.clone(), RunState::Ended(end.clone()));
        *runtime.latest_run.lock().unwrap() = Some(run_id.clone());
        let app = application_with_runtime(
            runtime.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );

        let result = app
            .drive_session_event_batches(&SessionId::from(session_id), None)
            .await;
        assert!(result.is_ok(), "U1-U6 reconciliation remains retryable");
        let persisted = repository
            .get(&SessionId::from(session_id))
            .await
            .expect("U1-U3 root");
        let entry = &persisted.event_batches[0].events[0];
        assert!(!entry.processed, "U1-U6/E1");
        assert!(entry.dispatch_settlement.is_none(), "U1-U6/E1");
        assert!(runtime.committed.lock().unwrap().is_empty(), "U1-U3/E2");
        let recovered = application_with_runtime(
            runtime.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        assert!(
            recovered
                .drive_session_event_batches(&SessionId::from(session_id), None)
                .await
                .is_ok()
        );
        assert_eq!(
            repository
                .get(&SessionId::from(session_id))
                .await
                .unwrap()
                .event_batches,
            persisted.event_batches,
            "retry preserves the exact root receipt and anchor"
        );
        assert_eq!(
            runtime.states.lock().unwrap().get(&run_id.0),
            Some(&RunState::Ended(end)),
            "U1-U3/E3 processed receipt never changes the unknown or failed outcome"
        );
        assert!(
            runtime.reserved.lock().unwrap().is_empty(),
            "U1-U3/E2 no admission replay"
        );
        assert!(
            runtime.activated.lock().unwrap().is_empty(),
            "U1-U3/E2 no prompt replay"
        );
    }
}

#[tokio::test]
async fn running_input_requires_exact_content_and_a_valid_committed_coordinate() {
    // Decision table over recovery evidence: I1 absent/foreign identity ->
    // pending without anchor; I2 exact identity + wrong role/content -> error;
    // I3 exact content + zero/future/unaligned cursor -> error; I4 exact valid
    // identity/content/cursor -> anchor only. Every row preserves the command,
    // performs no new Run admission, and never marks it processed.
    for evidence in [
        "missing",
        "foreign",
        "wrong_role",
        "wrong_content",
        "zero",
        "future",
        "unaligned",
        "valid",
    ] {
        let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let session_id = "running-input-evidence";
        let session = planned_session(
            session_id,
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("original")],
            }],
        );
        let (run_id, operation_id) = match &session.event_batches[0].events[0].event {
            SessionEventCommand::UserMessage {
                run_id,
                operation_id,
                ..
            } => (run_id.clone(), operation_id.clone()),
            _ => unreachable!(),
        };
        create(repository.as_ref(), session).await;
        let runtime = Arc::new(EventBatchRuntime::default());
        runtime
            .states
            .lock()
            .unwrap()
            .insert(run_id.0.clone(), RunState::Running);
        *runtime.latest_run.lock().unwrap() = Some(run_id);
        if evidence != "missing" {
            runtime.committed.lock().unwrap().push(Message::new(
                if evidence == "foreign" {
                    MessageId("other".into())
                } else {
                    MessageId::session_event_input(session_id, &operation_id)
                },
                if evidence == "wrong_role" {
                    Role::Assistant
                } else {
                    Role::User
                },
                vec![ContentBlock::text(if evidence == "wrong_content" {
                    "changed"
                } else {
                    "original"
                })],
            ));
        }
        *runtime.recovery_projection_override.lock().unwrap() = match evidence {
            "zero" => Some((vec![0], 1)),
            "future" => Some((vec![2], 1)),
            "unaligned" => Some((vec![], 1)),
            _ => None,
        };
        let app = application_with_runtime(
            runtime.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let outcome = app
            .drive_session_event_batches(&SessionId::from(session_id), None)
            .await;
        assert_eq!(
            outcome.is_ok(),
            matches!(evidence, "missing" | "foreign" | "valid"),
            "{evidence}"
        );
        let root = repository.get(&SessionId::from(session_id)).await.unwrap();
        let entry = &root.event_batches[0].events[0];
        assert_eq!(
            entry.projection_anchor.is_some(),
            evidence == "valid",
            "{evidence}"
        );
        assert!(
            !entry.processed && entry.processed_at_unix_ms.is_none(),
            "{evidence}"
        );
        assert!(
            runtime.reserved.lock().unwrap().is_empty()
                && runtime.activated.lock().unwrap().is_empty(),
            "{evidence}"
        );
    }
}

#[tokio::test]
async fn running_input_anchor_recovers_root_write_faults_without_reexecution() {
    // Causes: C1 exact input committed while R is Running; C2 root CAS normal,
    // unavailable before commit, or applied with lost acknowledgement; C3 two
    // cold supervisors race; C4 R subsequently reaches Awaiting. Effects:
    // E1 only visibility advances, no processed time or next User activation;
    // E2 failed CAS retains pending input, retries preserve the first anchor;
    // E3 exact replay is revision-inert; E4 only C4 completes the input.
    // Decision rules: R1=C1+normal -> E1; R2=C1+unavailable -> E2;
    // R3=C1+lost ack -> E1+E2; R4=R1-R3+C3 -> E1-E3; R5=R4+C4 -> E4.
    for fault in ["none", "unavailable", "lost_ack"] {
        let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let faulting = Arc::new(FaultingSessionRepository::new(repository.clone()));
        let session_id = "running-input-anchor";
        let mut session = planned_session(
            session_id,
            vec![
                SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("first")],
                },
                SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("queued")],
                },
            ],
        );
        // The original create wake has already handed activity to the Run.
        session.event_batches[0].wake_activity_epoch = None;
        let (run_id, operation_id) = match &session.event_batches[0].events[0].event {
            SessionEventCommand::UserMessage {
                run_id,
                operation_id,
                ..
            } => (run_id.clone(), operation_id.clone()),
            _ => unreachable!(),
        };
        create(repository.as_ref(), session).await;
        let runtime = Arc::new(EventBatchRuntime::default());
        runtime
            .states
            .lock()
            .unwrap()
            .insert(run_id.0.clone(), RunState::Running);
        *runtime.latest_run.lock().unwrap() = Some(run_id.clone());
        runtime.committed.lock().unwrap().push(Message::new(
            MessageId::session_event_input(session_id, &operation_id),
            Role::User,
            vec![ContentBlock::text("first")],
        ));
        let app = application_with_runtime(
            runtime.clone(),
            faulting.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        match fault {
            "unavailable" => faulting.fail_once("anchor-event-input"),
            "lost_ack" => faulting.commit_then_conflict_once("anchor-event-input"),
            _ => {}
        }
        let first = app
            .drive_session_event_batches(&SessionId::from(session_id), None)
            .await;
        assert_eq!(first.is_err(), fault == "unavailable", "R1-R3/{fault}");
        let observed = repository.get(&SessionId::from(session_id)).await.unwrap();
        assert_eq!(
            observed.event_batches[0].events[0]
                .projection_anchor
                .is_some(),
            fault != "unavailable"
        );
        let cold = application_with_runtime(
            runtime.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let joined_session = SessionId::from(session_id);
        let (a, b) = tokio::join!(
            app.drive_session_event_batches(&joined_session, None),
            cold.drive_session_event_batches(&joined_session, None)
        );
        a.unwrap();
        b.unwrap();
        let anchored = repository.get(&SessionId::from(session_id)).await.unwrap();
        assert_eq!(
            anchored.event_batches[0].events[0]
                .projection_anchor
                .unwrap()
                .source_commit_cursor,
            1,
            "R4"
        );
        assert!(
            anchored.event_batches[0]
                .events
                .iter()
                .all(|entry| !entry.processed && entry.processed_at_unix_ms.is_none()),
            "R4/E1"
        );
        assert!(
            runtime.reserved.lock().unwrap().is_empty()
                && runtime.activated.lock().unwrap().is_empty(),
            "R4 no duplicate or successor execution"
        );
        cold.drive_session_event_batches(&SessionId::from(session_id), None)
            .await
            .unwrap();
        assert_eq!(
            repository.get(&SessionId::from(session_id)).await.unwrap(),
            anchored,
            "R4/E3"
        );
        runtime.await_run(&run_id);
        cold.drive_session_event_batches(&SessionId::from(session_id), None)
            .await
            .unwrap();
        let completed = repository.get(&SessionId::from(session_id)).await.unwrap();
        assert!(!completed.event_batches[0].events[0].processed, "R5");
        assert!(
            completed.event_batches[0].events[0]
                .processed_at_unix_ms
                .is_none(),
            "R5"
        );
        assert_eq!(
            completed.event_batches[0].events[0].projection_anchor,
            anchored.event_batches[0].events[0].projection_anchor,
            "R5 immutable order"
        );
        assert!(
            !completed.event_batches[0].events[1].processed,
            "R5 awaiting still blocks next User"
        );
    }
}

#[tokio::test]
async fn unreplayed_receipt_requires_its_exact_terminal_user_run() {
    // Public reconciliation decision table: A1 Running with committed input
    // anchors visibility only; A2 Running without input remains unanchored; A3
    // Awaiting/Indeterminate with or without input remains unprocessed until an
    // applied Dispatch callback; A4 paired System remains pending. Thread truth
    // never substitutes for the post-Dispatch receipt.
    for state in [
        RunState::Running,
        RunState::Awaiting,
        RunState::Ended(EndCause::Indeterminate),
    ] {
        for committed_input in [false, true] {
            let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
            );
            let session_id = "exact-anchor-session";
            let session = planned_session(
                session_id,
                vec![
                    SessionEventInput::UserMessage {
                        content: vec![ContentBlock::text("accepted input")],
                    },
                    SessionEventInput::SystemMessage {
                        content: vec![ContentBlock::text("paired system input")],
                    },
                ],
            );
            let (run_id, message_id) = match &session.event_batches[0].events[0].event {
                SessionEventCommand::UserMessage {
                    run_id,
                    operation_id,
                    ..
                } => (
                    run_id.clone(),
                    MessageId::session_event_input(session_id, operation_id),
                ),
                _ => unreachable!("one retained User owner"),
            };
            create(repository.as_ref(), session).await;
            let runtime = Arc::new(EventBatchRuntime::default());
            let other_id = RunId("other-indeterminate-run".into());
            runtime
                .states
                .lock()
                .unwrap()
                .insert(other_id.0.clone(), RunState::Ended(EndCause::Indeterminate));
            runtime
                .states
                .lock()
                .unwrap()
                .insert(run_id.0.clone(), state.clone());
            *runtime.latest_run.lock().unwrap() = Some(other_id);
            if committed_input {
                let mut messages = runtime.committed.lock().unwrap();
                messages.push(Message::new(
                    message_id,
                    Role::User,
                    vec![ContentBlock::text("accepted input")],
                ));
                messages.push(Message::new(
                    MessageId::session_system(session_id, "later-unrelated"),
                    Role::System,
                    vec![ContentBlock::text("later unrelated commit")],
                ));
            }
            let before = runtime.committed.lock().unwrap().clone();
            let app = application_with_runtime(
                runtime.clone(),
                repository.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
            );
            let result = app
                .drive_session_event_batches(&SessionId::from(session_id), None)
                .await;
            assert!(result.is_ok(), "A1-A4 remain retryable before Dispatch");
            let persisted = repository.get(&SessionId::from(session_id)).await.unwrap();
            let processed = false;
            let entries = &persisted.event_batches[0].events;
            assert_eq!(
                entries[0].processed, processed,
                "A1-A3 exact User state/input"
            );
            assert_eq!(
                entries[0].projection_anchor.map(|a| a.source_commit_cursor),
                (state == RunState::Running && committed_input).then_some(1),
                "A1 original Message only; terminal truth does not publish"
            );
            assert_eq!(
                entries[0].processed_at_unix_ms.is_some(),
                processed,
                "A3 committed input is visibility evidence, not Run completion"
            );
            assert!(
                !entries[1].processed,
                "A4 System requires its own committed Message"
            );
            assert_eq!(
                *runtime.committed.lock().unwrap(),
                before,
                "A1-A4 no invented transcript"
            );
            assert!(
                runtime.reserved.lock().unwrap().is_empty(),
                "A1-A4 no admission replay"
            );
            assert!(
                runtime.activated.lock().unwrap().is_empty(),
                "A1-A4 no prompt replay"
            );
        }
    }
}

#[tokio::test]
async fn event_batch_idempotency_replays_the_root_receipt_across_restart() {
    // Cause/effect graph: C1 a key is absent/exact/conflicting; C2 the
    // application instance is warm or reconstructed over the same repository.
    // Effects: E1 absent appends one root batch; E2 exact returns that byte-for-
    // value batch without a new revision; E3 conflict fails closed; E4 a new key
    // remains a distinct user intent. Constraints: the Session root's retained
    // batch is the only receipt and request fingerprint owner.
    // | Rule | Key/fingerprint | Instance | Effect |
    // | I1 | new K1/F1 | warm | E1 |
    // | I2 | K1/F1 | warm/restarted | E2 |
    // | I3 | K1/F2 | any | E3 |
    // | I4 | K2/F1 | any | E4 |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("idempotency repository"),
    );
    create(
        repository.as_ref(),
        persisted("event-idempotency", false, "idle"),
    )
    .await;
    let input = vec![SessionEventInput::UserMessage {
        content: vec![ContentBlock::text("one intent")],
    }];
    let app = application_with_runtime(
        Arc::new(EventBatchRuntime::default()),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let first = app
        .append_session_event_batch_idempotent(
            &SessionId::from("event-idempotency"),
            input.clone(),
            None,
            None,
            Some(("key-1".into(), "fingerprint-1".into())),
        )
        .await
        .expect("I1/E1");
    let accepted_revision = repository
        .get(&SessionId::from("event-idempotency"))
        .await
        .unwrap()
        .revision;
    let replay = app
        .append_session_event_batch_idempotent(
            &SessionId::from("event-idempotency"),
            input.clone(),
            None,
            None,
            Some(("key-1".into(), "fingerprint-1".into())),
        )
        .await
        .expect("I2/E2 warm replay");
    assert_eq!(replay, first, "I2/E2");
    let warm_replay = repository
        .get(&SessionId::from("event-idempotency"))
        .await
        .unwrap();
    assert_eq!(warm_replay.revision, accepted_revision, "I2/E2 stutters");
    assert_eq!(warm_replay.event_batches.len(), 1, "I2/E2");

    let restarted = application_with_runtime(
        Arc::new(EventBatchRuntime::default()),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert_eq!(
        restarted
            .session_event_batch_idempotency(
                &SessionId::from("event-idempotency"),
                "key-1",
                "fingerprint-1",
            )
            .await
            .expect("I2 restart read"),
        SessionEventBatchIdempotency::Exact(first.clone()),
        "I2/E2 restart replay"
    );
    let cold_replay = restarted
        .append_session_event_batch_idempotent(
            &SessionId::from("event-idempotency"),
            input.clone(),
            None,
            None,
            Some(("key-1".into(), "fingerprint-1".into())),
        )
        .await
        .expect("I2/E2 cold retry");
    assert_eq!(cold_replay, first, "I2/E2 cold retry receipt");
    let cold_snapshot = repository
        .get(&SessionId::from("event-idempotency"))
        .await
        .unwrap();
    assert_eq!(cold_snapshot.revision, accepted_revision, "I2/E2 stutters");
    assert_eq!(cold_snapshot.event_batches.len(), 1, "I2/E2 appends once");
    let conflict = restarted
        .append_session_event_batch_idempotent(
            &SessionId::from("event-idempotency"),
            input.clone(),
            None,
            None,
            Some(("key-1".into(), "fingerprint-2".into())),
        )
        .await
        .expect_err("I3/E3");
    assert_eq!(conflict.code, "idempotency_conflict", "I3/E3");

    let later = restarted
        .append_session_event_batch_idempotent(
            &SessionId::from("event-idempotency"),
            input,
            None,
            None,
            Some(("key-2".into(), "fingerprint-1".into())),
        )
        .await
        .expect("I4/E4");
    assert_ne!(later.batch_id, first.batch_id, "I4/E4");
}

#[tokio::test]
async fn event_batch_lost_cas_ack_reloads_the_retained_admission_plan() {
    // Cause/effect decision table: C1 the sole root CAS commits an absent key;
    // C2 its acknowledgement is reported as Conflict; C3 retry reload sees the
    // exact accepted receipt. R1 C1+C2+C3=>return the retained batch, keep one
    // append, and advance the root exactly once. This exercises the production
    // retry loop; database atomicity/durability remains repository TCB.
    let durable: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("lost acknowledgement repository"),
    );
    create(durable.as_ref(), persisted("event-lost-ack", false, "idle")).await;
    let before = durable
        .get(&SessionId::from("event-lost-ack"))
        .await
        .unwrap()
        .revision;
    let faulting = Arc::new(FaultingSessionRepository::new(durable.clone()));
    faulting.commit_then_conflict_once("append-event-batch");
    let app = application_with_runtime(
        Arc::new(EventBatchRuntime::default()),
        faulting,
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let admitted = app
        .append_session_event_batch_idempotent(
            &SessionId::from("event-lost-ack"),
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("one intent")],
            }],
            None,
            None,
            Some(("key".into(), "fingerprint".into())),
        )
        .await
        .expect("R1 reloads the committed winner");

    let retained = durable
        .get(&SessionId::from("event-lost-ack"))
        .await
        .unwrap();
    assert_eq!(retained.event_batches, vec![admitted], "R1 one append");
    assert_eq!(retained.revision.0, before.0 + 1, "R1 one root advance");
}

#[tokio::test]
async fn final_event_admission_close_is_absorbing_across_restart_and_late_delivery() {
    // Causes: C1 accept/close wins the same root CAS; C2 response is lost and a
    // new application instance resolves; C3 delayed delivery is exact/changed.
    // Effects: E1 accepted intent is never closed; E2 closed intent never runs;
    // E3 exact resolution reuses one root revision; E4 changed body conflicts.
    // Rules F1 accept first=>E1+E3; F2 close first=>E2+E3;
    // F3 F1/F2 + restart=>same receipt; F4 changed fingerprint=>E4.
    for accept_first in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("admission.db");
        let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(
                database_path.to_str().unwrap(),
            )
            .unwrap(),
        );
        let session = "final-admission";
        create(repository.as_ref(), persisted(session, false, "idle")).await;
        let runtime = Arc::new(EventBatchRuntime::default());
        let app = application_with_runtime(
            runtime.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let input = vec![SessionEventInput::UserMessage {
            content: vec![ContentBlock::text("original request")],
        }];
        let key = Some(("key".to_owned(), "fingerprint".to_owned()));
        if accept_first {
            app.append_session_event_batch_idempotent(
                &SessionId::from(session),
                input.clone(),
                None,
                None,
                key.clone(),
            )
            .await
            .unwrap();
        }
        let first = app
            .close_session_event_admission(
                "workspace",
                &SessionId::from(session),
                "key".into(),
                "fingerprint".into(),
            )
            .await
            .unwrap();
        assert_eq!(
            matches!(
                first,
                awaken_session_contract::SessionEventAdmission::Closed { .. }
            ),
            !accept_first,
            "F1/F2"
        );
        let revision = repository
            .get(&SessionId::from(session))
            .await
            .unwrap()
            .revision;
        // Reopen the actual database, not merely a new application over the
        // old in-memory connection; response-loss proof must survive restart.
        drop(app);
        drop(repository);
        let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(
                database_path.to_str().unwrap(),
            )
            .unwrap(),
        );
        let restarted = application_with_runtime(
            runtime.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let replay = restarted
            .close_session_event_admission(
                "workspace",
                &SessionId::from(session),
                "key".into(),
                "fingerprint".into(),
            )
            .await
            .unwrap();
        assert_eq!(first, replay, "F3 stable receipt after reconstruction");
        let late = restarted
            .append_session_event_batch_idempotent(
                &SessionId::from(session),
                input,
                None,
                None,
                key,
            )
            .await;
        if accept_first {
            assert_eq!(
                awaken_session_contract::SessionEventAdmission::from(&late.unwrap()),
                first,
                "F1 no duplicate acceptance"
            );
        } else {
            assert_eq!(
                late.unwrap_err().code,
                "event_admission_closed",
                "F2 late request fenced"
            );
            let batch = &repository
                .get(&SessionId::from(session))
                .await
                .unwrap()
                .event_batches[0];
            assert!(
                batch.is_complete() && batch.events.is_empty(),
                "F2 no unfinished execution intent"
            );
            restarted
                .drive_session_event_batches(&SessionId::from(session), None)
                .await
                .unwrap();
            assert!(
                runtime.reserved.lock().unwrap().is_empty(),
                "F2 no Run entered"
            );
        }
        let conflict = restarted
            .close_session_event_admission(
                "workspace",
                &SessionId::from(session),
                "key".into(),
                "changed".into(),
            )
            .await
            .unwrap_err();
        assert_eq!(conflict.code, "idempotency_conflict", "F4");
        let retained = repository.get(&SessionId::from(session)).await.unwrap();
        assert_eq!(retained.revision, revision, "F3/F4 no new durable mutation");
        assert_eq!(retained.event_batches.len(), 1, "one identity owner");
    }
}

#[tokio::test]
async fn final_event_admission_close_competes_atomically_with_acceptance() {
    // F5: two replicas race accept versus final close under the existing root
    // CAS. Exactly one retained identity wins. Accepted=>close returns accepted;
    // closed=>accept rejects. An absent read alone is deliberately not a proof.
    let repository: Arc<dyn ManagedSessionRepository> =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(
        repository.as_ref(),
        persisted("admission-race", false, "idle"),
    )
    .await;
    let a = application_with_runtime(
        Arc::new(EventBatchRuntime::default()),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let b = application_with_runtime(
        Arc::new(EventBatchRuntime::default()),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let joined_session = SessionId::from("admission-race");
    let (accepted, resolved) = tokio::join!(
        a.append_session_event_batch_idempotent(
            &joined_session,
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("one request")],
            }],
            None,
            None,
            Some(("key".into(), "fingerprint".into()))
        ),
        b.close_session_event_admission(
            "workspace",
            &joined_session,
            "key".into(),
            "fingerprint".into()
        ),
    );
    let resolved = resolved.unwrap();
    if matches!(
        resolved,
        awaken_session_contract::SessionEventAdmission::Closed { .. }
    ) {
        assert_eq!(
            accepted.unwrap_err().code,
            "event_admission_closed",
            "F5 close won"
        );
    } else {
        assert_eq!(
            awaken_session_contract::SessionEventAdmission::from(&accepted.unwrap()),
            resolved,
            "F5 acceptance won"
        );
    }
    assert_eq!(
        repository
            .get(&SessionId::from("admission-race"))
            .await
            .unwrap()
            .event_batches
            .iter()
            .map(awaken_session_contract::SessionEventAdmission::from)
            .collect::<Vec<_>>(),
        vec![resolved],
        "F5 one root receipt"
    );
}

#[tokio::test]
async fn competing_tool_replies_recheck_occupancy_after_root_cas_conflict() {
    competing_tool_reply_admission_table(Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
    ))
    .await;
}

#[tokio::test]
async fn postgres_competing_tool_replies_recheck_occupancy_after_root_cas_conflict() {
    // Same R1-R4 rules through the production PostgreSQL root CAS. An explicit
    // unavailable database is fatal; an unconfigured run claims no PG coverage.
    let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
        return;
    };
    let repository = awaken_session_store::PostgresManagedSessionRepository::connect(&url)
        .await
        .expect("configured tool-reply admission PostgreSQL");
    competing_tool_reply_admission_table(Arc::new(repository)).await;
}

async fn competing_tool_reply_admission_table(repository: Arc<dyn ManagedSessionRepository>) {
    // Cause/effect graph: two replicas read the SAME old root; different HTTP
    // keys answer one pending occurrence. A barrier at the real pre-CAS Runtime
    // read forces this window; no scheduler-luck assumption. R1 equal payload,
    // R2 changed payload: exactly one receipt + one rejection + one root batch.
    // R3 exact winning-key replay returns that receipt despite occupied call.
    // R4 a new key after admission rejects without changing root/outbox. The
    // rejected caller must not reach Runtime delivery in any row.
    for changed in [false, true] {
        let session_id = format!("reply-cas-race-{}-{changed}", std::process::id());
        let session = session_id.as_str();
        create(repository.as_ref(), persisted(session, false, "idle")).await;
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let runtime_a = Arc::new(EventBatchRuntime::default());
        let runtime_b = Arc::new(EventBatchRuntime::default());
        *runtime_a.admission_barrier.lock().unwrap() = Some(barrier.clone());
        *runtime_b.admission_barrier.lock().unwrap() = Some(barrier);
        let a = application_with_runtime(
            runtime_a.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let b = application_with_runtime(
            runtime_b.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let left = tool_reply_input("one-call");
        let mut right = left.clone();
        if changed {
            let SessionEventInput::ToolReply(reply) = &mut right else {
                unreachable!()
            };
            reply.reply = SessionEventToolReplyKind::ToolResult {
                content: Some(vec![ContentBlock::text("other")]),
                is_error: true,
            };
        }
        let joined_session = SessionId::from(session);
        let (ra, rb) = tokio::join!(
            a.append_session_event_batch_idempotent(
                &joined_session,
                vec![left.clone()],
                None,
                None,
                Some(("a".into(), "fa".into()))
            ),
            b.append_session_event_batch_idempotent(
                &joined_session,
                vec![right.clone()],
                None,
                None,
                Some(("b".into(), "fb".into()))
            )
        );
        let outcomes = [ra, rb];
        assert_eq!(
            outcomes.iter().filter(|v| v.is_ok()).count(),
            1,
            "R1/R2 one winner"
        );
        assert_eq!(
            outcomes.iter().find_map(|v| v.as_ref().err()).unwrap().kind,
            awaken_session_contract::RunErrorKind::BadRequest
        );
        let winner = outcomes.iter().find_map(|v| v.as_ref().ok()).unwrap();
        let (input, key, fp) = if outcomes[0].is_ok() {
            (left.clone(), "a", "fa")
        } else {
            (right, "b", "fb")
        };
        let before = repository.get(&SessionId::from(session)).await.unwrap();
        let outbox = repository.pending_lifecycle().await.unwrap();
        assert_eq!(before.event_batches.len(), 1);
        assert_eq!(
            &a.append_session_event_batch_idempotent(
                &SessionId::from(session),
                vec![input],
                None,
                None,
                Some((key.into(), fp.into()))
            )
            .await
            .unwrap(),
            winner,
            "R3 exact receipt"
        );
        assert!(
            b.append_session_event_batch_idempotent(
                &SessionId::from(session),
                vec![left],
                None,
                None,
                Some(("late".into(), "fl".into()))
            )
            .await
            .is_err(),
            "R4 occupied"
        );
        assert_eq!(
            repository.get(&SessionId::from(session)).await.unwrap(),
            before,
            "R3/R4 root frame"
        );
        assert_eq!(
            repository.pending_lifecycle().await.unwrap(),
            outbox,
            "R3/R4 outbox frame"
        );
        assert_eq!(
            runtime_a.tool_reply_calls.load(Ordering::SeqCst)
                + runtime_b.tool_reply_calls.load(Ordering::SeqCst),
            0
        );
    }
}

#[tokio::test]
async fn concurrent_event_batch_idempotency_converges_under_root_cas() {
    // Cause/effect graph: C1 two application replicas share one Session root;
    // C2 requests race with one key and equal or different fingerprints.
    // Effects: E1 equal requests return one canonical batch to both callers;
    // E2 different requests select exactly one winner and one conflict; E3 the
    // root retains one batch in either case. Constraint: no process-local lock
    // or secondary receipt store participates; repository CAS is the arbiter.
    // | Rule | Replicas | Key | Fingerprints | Effects |
    // | C1 | two | same | equal | E1+E3 |
    // | C2 | two | same | different | E2+E3 |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("concurrent idempotency repository"),
    );
    for session_id in ["event-race-equal", "event-race-conflict"] {
        create(repository.as_ref(), persisted(session_id, false, "idle")).await;
    }
    let replica_a = Arc::new(application_with_runtime(
        Arc::new(EventBatchRuntime::default()),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let replica_b = Arc::new(application_with_runtime(
        Arc::new(EventBatchRuntime::default()),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let input = vec![SessionEventInput::UserMessage {
        content: vec![ContentBlock::text("concurrent intent")],
    }];

    let joined_session = SessionId::from("event-race-equal");
    let (equal_a, equal_b) = tokio::join!(
        replica_a.append_session_event_batch_idempotent(
            &joined_session,
            input.clone(),
            None,
            None,
            Some(("shared-key".into(), "same-fingerprint".into())),
        ),
        replica_b.append_session_event_batch_idempotent(
            &joined_session,
            input.clone(),
            None,
            None,
            Some(("shared-key".into(), "same-fingerprint".into())),
        ),
    );
    let equal_a = equal_a.expect("C1/E1 replica A");
    let equal_b = equal_b.expect("C1/E1 replica B");
    assert_eq!(equal_a, equal_b, "C1/E1 one canonical receipt");
    assert_eq!(
        repository
            .get(&SessionId::from("event-race-equal"))
            .await
            .unwrap()
            .event_batches
            .len(),
        1,
        "C1/E3 one root batch",
    );

    let joined_session = SessionId::from("event-race-conflict");
    let (different_a, different_b) = tokio::join!(
        replica_a.append_session_event_batch_idempotent(
            &joined_session,
            input.clone(),
            None,
            None,
            Some(("shared-key".into(), "fingerprint-a".into())),
        ),
        replica_b.append_session_event_batch_idempotent(
            &joined_session,
            input,
            None,
            None,
            Some(("shared-key".into(), "fingerprint-b".into())),
        ),
    );
    let outcomes = [different_a, different_b];
    assert_eq!(
        outcomes.iter().filter(|result| result.is_ok()).count(),
        1,
        "C2/E2 winner"
    );
    let conflict = outcomes
        .iter()
        .find_map(|result| result.as_ref().err())
        .expect("C2/E2 loser conflicts");
    assert_eq!(conflict.code, "idempotency_conflict", "C2/E2");
    assert_eq!(
        repository
            .get(&SessionId::from("event-race-conflict"))
            .await
            .unwrap()
            .event_batches
            .len(),
        1,
        "C2/E3 one root batch",
    );
}

#[tokio::test]
async fn legacy_terminal_batches_resolve_under_root_cas_without_runtime_effects() {
    // Cause/effect graph: C1 a terminal root retains three ordered entries:
    // processed(time=50, input anchor=7, Dispatch commit anchor=11),
    // pending(anchor=9), pending(no anchor). The first receipt is constructed
    // by the real aggregate settlement after opening its exact activity; this
    // fixture represents an applied Dispatch fact, not an actual Dispatch CAS.
    // C2 cleanup is Requested(Some(17)), legacy Requested(None), or Fenced
    // followed by a real freeze CAS(Some(23)). C3 resolution CAS succeeds,
    // fails Unavailable before SQL, or commits in SQL then reports Conflict.
    // C4 a reconstructed application over the same SQLite connection retries
    // at a later clock (not a process crash or disk reopen). T1 Fenced => complete
    // root/outbox stutter and no Runtime effect. T2 Requested + success/lost
    // acknowledgement => exactly one root revision, only pending entries gain
    // first time; prior anchors and every other aggregate field are retained.
    // T3 precommit failure => complete stutter; later retry establishes the
    // first observation at its own clock. T4 all resolved + C4 => complete
    // root/outbox stutter even on direct drive (not merely an empty scan).
    // The 3 cleanup paths x 3 CAS outcomes reuse the existing SQLite decorator,
    // application clock, aggregate constructor and supervisor. Expected frames
    // are explicit field deltas, never computed with resolve_terminally itself.
    use awaken_agent_contract::clock::ManualClock;
    use awaken_session_contract::SessionEventProjectionAnchor;
    for cleanup in ["requested", "legacy", "fenced"] {
        for fault in ["none", "unavailable", "lost_ack"] {
            let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
            );
            let faulting = Arc::new(FaultingSessionRepository::new(repository.clone()));
            let session_id = format!("terminal-batch-{cleanup}-{fault}");
            let mut session = planned_session(
                &session_id,
                [
                    "already observed",
                    "input anchored",
                    "awaiting terminal anchor",
                ]
                .into_iter()
                .map(|text| SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text(text)],
                })
                .collect(),
            );
            let batch = &mut session.event_batches[0];
            let first = batch.events[0].event.operation_id().to_owned();
            let first_run = match &batch.events[0].event {
                SessionEventCommand::UserMessage { run_id, .. } => run_id.clone(),
                _ => unreachable!("the fixture compiles a User command"),
            };
            let second = batch.events[1].event.operation_id().to_owned();
            batch
                .anchor_committed_input(
                    &first,
                    SessionEventProjectionAnchor {
                        source_commit_cursor: 7,
                    },
                )
                .unwrap();
            batch
                .anchor_committed_input(
                    &second,
                    SessionEventProjectionAnchor {
                        source_commit_cursor: 9,
                    },
                )
                .unwrap();
            let epoch = session.begin_activity_epoch().unwrap();
            assert!(matches!(
                session
                    .settle_user_event_dispatch(
                        &first_run,
                        awaken_session_contract::SessionAgentRunBoundary::Completed,
                        epoch,
                        SessionEventProjectionAnchor {
                            source_commit_cursor: 11
                        },
                        50,
                    )
                    .unwrap(),
                awaken_session_contract::SessionEventDispatchSettlementEffect::Applied {
                    fully_settled: false
                }
            ));
            assert!(
                session.event_batches[0].events[0]
                    .dispatch_settlement
                    .is_some()
            );
            session
                .transition_execution(awaken_session_contract::SessionExecutionState::Terminated)
                .unwrap();
            assert!(session.terminal_cleanup.request(&session_id));
            if cleanup == "requested" {
                session
                    .terminal_cleanup
                    .freeze_targets(&session_id, [], 0, 17)
                    .unwrap();
            } else if cleanup == "legacy" {
                let mut legacy = serde_json::to_value(&session).unwrap();
                legacy["terminal_cleanup"]["state"] = serde_json::Value::String("requested".into());
                session = serde_json::from_value(legacy).unwrap();
            }
            create(repository.as_ref(), session).await;
            let runtime = Arc::new(EventBatchRuntime::default());
            let clock = Arc::new(ManualClock::new(100));
            let app = application_with_runtime(
                runtime.clone(),
                faulting.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
            )
            .with_clock(clock.clone());
            let initial = repository.get(&SessionId::from(&session_id)).await.unwrap();
            let outbox =
                serde_json::to_value(repository.pending_lifecycle().await.unwrap()).unwrap();
            if cleanup == "fenced" {
                let report = app.reconcile_event_batches().await;
                assert!(report.failures.is_empty(), "T1/{fault}");
                assert_eq!(report.settled, 0, "T1/{fault}");
                assert_eq!(
                    repository.get(&SessionId::from(&session_id)).await.unwrap(),
                    initial,
                    "T1 complete fenced frame"
                );
                let mut frozen = initial.clone();
                frozen
                    .terminal_cleanup
                    .freeze_targets(&session_id, [], 0, 23)
                    .unwrap();
                let expected_revision = frozen.revision;
                let payload = awaken_session_contract::SessionMutationPayload::Replace(frozen);
                let payload_hash = payload.stable_hash();
                assert!(matches!(
                    repository
                        .commit_mutation(
                            "workspace",
                            awaken_session_contract::SessionMutation {
                                expected_revision,
                                idempotency: awaken_session_contract::IdempotencyRecord {
                                    key: format!("freeze:{session_id}"),
                                    payload_hash
                                },
                                payload,
                                lifecycle_facts: Vec::new(),
                            }
                        )
                        .await
                        .unwrap(),
                    awaken_session_contract::SessionMutationResult::Applied { .. }
                ));
            }
            let before = repository.get(&SessionId::from(&session_id)).await.unwrap();
            match fault {
                "unavailable" => faulting.fail_once("resolve-terminal-event-batches"),
                "lost_ack" => faulting.commit_then_conflict_once("resolve-terminal-event-batches"),
                _ => {}
            }
            let report = app.reconcile_event_batches().await;
            let first_time = if fault == "unavailable" {
                assert_eq!(report.failures.len(), 1, "T3 exact failed root CAS");
                assert!(
                    report.failures[0].1.contains("injected root-CAS outage"),
                    "T3 original infrastructure error"
                );
                assert_eq!(report.settled, 0, "T3 no success before commit");
                assert_eq!(
                    repository.get(&SessionId::from(&session_id)).await.unwrap(),
                    before,
                    "T3 complete failed frame"
                );
                assert_eq!(
                    serde_json::to_value(repository.pending_lifecycle().await.unwrap()).unwrap(),
                    outbox,
                    "T3 outbox frame"
                );
                clock.set(200);
                let retry = app.reconcile_event_batches().await;
                assert!(
                    retry.failures.is_empty(),
                    "T3 recovery: {:?}",
                    retry.failures
                );
                assert_eq!(retry.settled, 1, "T3 one recovery");
                200
            } else {
                assert!(
                    report.failures.is_empty(),
                    "T2/{cleanup}/{fault}: {:?}",
                    report.failures
                );
                assert_eq!(report.settled, 1, "T2 one resolution");
                100
            };
            let mut expected = before;
            expected.revision.0 += 1;
            expected.updated_at_unix_ms = Some(first_time);
            expected.event_batches[0].events[1].processed = true;
            expected.event_batches[0].events[1].processed_at_unix_ms = Some(first_time);
            expected.event_batches[0].events[2].processed = true;
            expected.event_batches[0].events[2].processed_at_unix_ms = Some(first_time);
            expected.event_batches[0].events[2].projection_anchor = match cleanup {
                "requested" => Some(SessionEventProjectionAnchor {
                    source_commit_cursor: 17,
                }),
                "fenced" => Some(SessionEventProjectionAnchor {
                    source_commit_cursor: 23,
                }),
                _ => None,
            };
            assert_eq!(
                repository.get(&SessionId::from(&session_id)).await.unwrap(),
                expected,
                "T2/T3 complete committed frame {cleanup}/{fault}"
            );
            let cold = application_with_runtime(
                runtime.clone(),
                repository.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
            )
            .with_clock(Arc::new(ManualClock::new(900)));
            let replay = cold.reconcile_event_batches().await;
            assert!(replay.failures.is_empty(), "T4 cold scan");
            assert_eq!(replay.settled, 0, "T4 no unfinished batch");
            cold.drive_session_event_batches(&SessionId::from(&session_id), None)
                .await
                .unwrap();
            assert_eq!(
                repository.get(&SessionId::from(session_id)).await.unwrap(),
                expected,
                "T4 complete replay frame {cleanup}/{fault}"
            );
            assert_eq!(
                serde_json::to_value(repository.pending_lifecycle().await.unwrap()).unwrap(),
                outbox,
                "T1-T4 outbox frame"
            );
            assert!(
                runtime.trace.lock().unwrap().is_empty(),
                "T1-T4 no Runtime effect"
            );
            assert!(
                runtime.reserved.lock().unwrap().is_empty(),
                "T1-T4 no reservation"
            );
            assert!(
                runtime.activated.lock().unwrap().is_empty(),
                "T1-T4 no activation"
            );
            assert!(
                runtime.outcomes.lock().unwrap().is_empty(),
                "T1-T4 no Outcome"
            );
            assert_eq!(
                runtime.tool_reply_calls.load(Ordering::SeqCst),
                0,
                "T1-T4 no tool reply"
            );
            assert_eq!(
                runtime.primary_interrupt_calls.load(Ordering::SeqCst),
                0,
                "T1-T4 no interrupt"
            );
        }
    }
}

#[test]
fn deleting_tombstone_waits_for_terminal_batch_provenance() {
    // Coverage rationale: the async case below owns D1-D3 and their E1-E3
    // oracle. The shared composed-test executor changes stack placement only;
    // it adds no scenario, state owner, or alternate assertion path.
    run_composed_async_test(deleting_tombstone_waits_for_terminal_batch_provenance_case);
}

async fn deleting_tombstone_waits_for_terminal_batch_provenance_case() {
    // Cause/effect graph: C1 an old writer left Deleting+cleanup-Completed with
    // one accepted incomplete Event entry; C2 resource reconciliation runs
    // before Event reconciliation; C3 the canonical Event supervisor resolves
    // the entry at the retained terminal cursor; C4 resource reconciliation
    // retries. Effects: E1 C2 retains the aggregate and its receipt without any
    // Runtime command effect; E2 C3 commits processed+anchor under root CAS; E3
    // only C4 replaces the now-complete aggregate with its compact tombstone.
    // Decision table: D1=C1+C2=>E1; D2=D1+C3=>E2; D3=D2+C4=>E3. Tombstone
    // admission and the supervisor share the existing Session root authority.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("deleting batch repository"),
    );
    let mut session = planned_session(
        "deleting-batch",
        vec![SessionEventInput::UserMessage {
            content: vec![ContentBlock::text("accepted before old delete")],
        }],
    );
    session
        .transition_execution(awaken_session_contract::SessionExecutionState::Terminated)
        .expect("legacy delete starts from a valid terminal aggregate");
    session.disposition = awaken_session_contract::SessionDisposition::Deleting;
    let mut legacy = serde_json::to_value(&session).unwrap();
    legacy["terminal_cleanup"] = serde_json::json!({
        "state": "completed",
        "effect_id": "legacy-delete-cleanup",
        "thread_ids": ["deleting-batch"],
        "delegation_watermark": 0,
        "runtime_commit_cursor": 31,
        "receipt_fingerprint": "legacy-delete-receipt"
    });
    session = serde_json::from_value(legacy)
        .expect("aggregate codec reads legacy completed terminal cleanup");
    create(repository.as_ref(), session).await;
    let runtime = Arc::new(EventBatchRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    // Each D1-D3 reconciliation owns a large generated future. Pin the three
    // decision rules independently so the test driver does not retain one
    // reconciler frame while SQLite decodes the next root snapshot.
    let first = Box::pin(app.reconcile_resource_activations()).await;
    assert!(first.failures.is_empty(), "D1/E1");
    assert!(
        !repository
            .get(&SessionId::from("deleting-batch"))
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "D1/E1 provenance survives the resource-first pass"
    );

    let events = Box::pin(app.reconcile_event_batches()).await;
    assert!(events.failures.is_empty(), "D2/E2: {:?}", events.failures);
    let resolved = repository
        .get(&SessionId::from("deleting-batch"))
        .await
        .unwrap();
    assert!(resolved.event_batches[0].events[0].processed, "D2/E2");
    assert_eq!(
        resolved.event_batches[0].events[0]
            .projection_anchor
            .map(|anchor| anchor.source_commit_cursor),
        Some(31),
        "D2/E2"
    );

    let second = Box::pin(app.reconcile_resource_activations()).await;
    assert!(second.failures.is_empty(), "D3/E3");
    assert!(matches!(
        repository.get(&SessionId::from("deleting-batch")).await,
        Err(awaken_session_contract::SessionRepositoryError::NotFound)
    ));
    assert!(runtime.trace.lock().unwrap().is_empty(), "D1-D3/E1");
    assert!(runtime.reserved.lock().unwrap().is_empty(), "D1-D3/E1");
}

#[tokio::test]
async fn immediate_commands_bypass_queued_user_and_reply_system_observation_is_narrow() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 a current Run is Awaiting; C2 an earlier accepted
    // User is queued; C3 later retained commands are ToolReply+System,
    // Interrupt(all frozen targets), and DefineOutcome; C4 the reply's System
    // Message is staged/not committed/committed. Effects: E1 the three receipt
    // commands cross C2 in retained order; E2 the exact reply command freezes
    // adjacent System in the same resume payload; E3 User remains queued; E4
    // System remains unprocessed until exact committed Message truth exists;
    // E5 only the System whose processed predecessor is ToolReply may then cross
    // C2 as a side-effect-free observation.
    //
    // | Rule | Awaiting | Queued User | Receipt/System truth | Effect |
    // | P1 | yes | yes | reply,interrupt,outcome | E1+E2+E3 |
    // | P2 | yes | yes | reply staged, System absent | E4 |
    // | P3 | yes | yes | exact System committed | E5; User still E3 |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.tool_reply_prior_epoch.store(1, Ordering::SeqCst);
    runtime.await_run(&RunId("awaiting-run".into()));
    *runtime.latest_run.lock().unwrap() = Some(RunId("awaiting-run".into()));
    create(
        repository.as_ref(),
        running_session_with_activity("receipt-priority", 1),
    )
    .await;
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let reply_batch = app
        .append_session_event_batch(
            "receipt-priority",
            vec![
                SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("queued")],
                },
                tool_reply_input("reply-tool"),
                SessionEventInput::SystemMessage {
                    content: vec![ContentBlock::text("reply context")],
                },
            ],
            Some("subject-queued".into()),
            None,
        )
        .await
        .expect("P1 reply batch");
    let interrupt_batch = app
        .append_session_event_batch(
            "receipt-priority",
            vec![SessionEventInput::Interrupt(SessionEventInterrupt {
                requested_target: None,
                targets: vec![
                    SessionThreadTarget::Primary,
                    SessionThreadTarget::Child(ThreadId("event-child".into())),
                ],
            })],
            None,
            None,
        )
        .await
        .expect("P1 interrupt batch");
    let outcome_batch = app
        .append_session_event_batch(
            "receipt-priority",
            vec![SessionEventInput::DefineOutcome {
                description: "ship".into(),
                rubric: SessionOutcomeRubric::Text {
                    content: "correct".into(),
                },
                max_iterations: Some(2),
            }],
            None,
            None,
        )
        .await
        .expect("P1 outcome batch");

    app.drive_session_event_batches(&SessionId::from("receipt-priority"), None)
        .await
        .expect("P1-P2 opportunistic drive");
    let effect_order = runtime
        .trace
        .lock()
        .unwrap()
        .iter()
        .filter_map(|entry| {
            entry
                .starts_with("reply:")
                .then_some("reply")
                .or_else(|| (entry == "interrupt:primary").then_some("interrupt-primary"))
                .or_else(|| (entry == "interrupt:event-child").then_some("interrupt-child"))
                .or_else(|| entry.starts_with("outcome:").then_some("outcome"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        effect_order,
        ["reply", "interrupt-primary", "interrupt-child", "outcome"],
        "P1/E1 retained receipt order"
    );
    let durable = repository
        .get(&SessionId::from("receipt-priority"))
        .await
        .unwrap();
    let reply = durable
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == reply_batch.batch_id)
        .expect("P1 reply provenance");
    assert!(!reply.events[0].processed, "P1/E3 queued User");
    assert!(reply.events[1].processed, "P1/E1 reply receipt");
    assert!(!reply.events[2].processed, "P2/E4 System not committed");
    assert!(
        durable
            .event_batches
            .iter()
            .find(|batch| batch.batch_id == interrupt_batch.batch_id)
            .unwrap()
            .events[0]
            .processed,
        "P1/E1 interrupt receipt"
    );
    assert!(
        durable
            .event_batches
            .iter()
            .find(|batch| batch.batch_id == outcome_batch.batch_id)
            .unwrap()
            .events[0]
            .processed,
        "P1/E1 outcome receipt"
    );
    {
        let staged_replies = runtime.staged_tool_replies.lock().unwrap();
        let staged = staged_replies.values().next().expect("P1/E2 staged reply");
        assert_eq!(staged.tool_use_id, "reply-tool", "P1/E2");
        assert_eq!(
            staged
                .accompanying_system
                .as_ref()
                .map(|system| system.content.clone()),
            Some(vec![ContentBlock::text("reply context")]),
            "P1/E2 exact adjacent System"
        );
    }

    runtime.commit_staged_system_messages();
    app.drive_session_event_batches(&SessionId::from("receipt-priority"), None)
        .await
        .expect("P3 committed System observation");
    let durable = repository
        .get(&SessionId::from("receipt-priority"))
        .await
        .unwrap();
    let reply = durable
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == reply_batch.batch_id)
        .unwrap();
    assert!(!reply.events[0].processed, "P3/E3 User remains queued");
    assert!(reply.events[2].processed, "P3/E5 exact System observed");
    assert!(runtime.reserved.lock().unwrap().is_empty(), "P1-P3/E3");
}

async fn recover_failed_reply_with_interrupt()
-> (Arc<dyn ManagedSessionRepository>, SessionApplication) {
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("CR1 repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.tool_reply_prior_epoch.store(1, Ordering::SeqCst);
    runtime
        .fail_tool_reply_after_stage_once
        .store(true, Ordering::SeqCst);
    create(
        repository.as_ref(),
        running_session_with_activity("cold-interrupt-priority", 1),
    )
    .await;
    let warm = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let reply = warm
        .append_session_event_batch(
            "cold-interrupt-priority",
            vec![
                tool_reply_input("older-damaged-reply"),
                SessionEventInput::SystemMessage {
                    content: vec![ContentBlock::text("superseded reply context")],
                },
            ],
            None,
            None,
        )
        .await
        .expect("CR1 older reply");
    let reply_activity_operation =
        tool_reply_activity_operation_id("cold-interrupt-priority", &reply);
    let interrupt = warm
        .append_session_event_batch(
            "cold-interrupt-priority",
            vec![SessionEventInput::Interrupt(SessionEventInterrupt {
                requested_target: None,
                targets: vec![
                    SessionThreadTarget::Child(ThreadId("event-child".into())),
                    SessionThreadTarget::Primary,
                ],
            })],
            None,
            None,
        )
        .await
        .expect("CR1 later interrupt");

    let cold = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let error = cold
        .drive_session_event_batches(&SessionId::from("cold-interrupt-priority"), None)
        .await
        .expect_err("CR1/E2 reply failure remains independent");
    assert_eq!(
        error.message, "scripted crash after durable reply stage",
        "CR1/E2 original reply retry signal"
    );
    assert_eq!(
        runtime.trace.lock().unwrap().as_slice(),
        [
            "reply:older-damaged-reply",
            "interrupt:event-child",
            "interrupt:primary",
        ],
        "CR1/E1 mixed targets include the exact primary reply target"
    );
    assert_eq!(
        runtime.primary_interrupt_calls.load(Ordering::SeqCst),
        1,
        "CR1/E1 cold recovery must execute Interrupt before retrying damaged ToolReply"
    );
    let repaired = repository
        .get(&SessionId::from("cold-interrupt-priority"))
        .await
        .unwrap();
    let reply = repaired
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == reply.batch_id)
        .expect("CR1 reply provenance");
    let interrupt = repaired
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == interrupt.batch_id)
        .expect("CR1 Interrupt provenance");
    assert!(reply.events[0].processed, "CR1/E2 reply superseded");
    assert!(
        reply.events[1].processed,
        "CR1/E2 adjacent System superseded"
    );
    assert!(interrupt.events[0].processed, "CR1/E2 Interrupt anchored");
    assert_eq!(
        [
            reply.events[0].projection_anchor,
            reply.events[1].projection_anchor,
            interrupt.events[0].projection_anchor,
        ],
        [interrupt.events[0].projection_anchor; 3],
        "CR1/E2 one Interrupt anchor owns the atomic supersession"
    );
    let (activity_root, reply_activity_epoch) = cold
        .recover_activity_for_operation(
            &SessionId::from("cold-interrupt-priority"),
            &reply_activity_operation,
        )
        .await
        .expect("CR1/E2 exact activity receipt remains recoverable")
        .expect("CR1/E2 reply transferred one exact activity epoch");
    assert_ne!(reply_activity_epoch, 1, "CR1/E2 transferred epoch is exact");
    assert!(
        !activity_root
            .active_activity_epochs
            .contains(&reply_activity_epoch),
        "CR1/E2 exact reply activity is settled before root supersession"
    );
    assert_eq!(
        activity_root.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "CR1/E4 recovery cannot leave a Running aggregate without pending work"
    );
    cold.drive_session_event_batches(&SessionId::from("cold-interrupt-priority"), None)
        .await
        .expect("CR1/E4 second cold scan has no damaged backlog");
    assert_eq!(runtime.tool_reply_calls.load(Ordering::SeqCst), 1, "CR1/E4");

    // The fixture retains staged reply payloads for response-loss replay; the
    // production Interrupt owner has already terminalized that Runtime await.
    // Remove only that fixture projection before exercising unrelated new work.
    runtime.staged_tool_replies.lock().unwrap().clear();
    (repository, cold)
}

#[tokio::test]
async fn cold_recovery_interrupt_is_not_starved_by_an_older_failed_tool_reply() {
    // Cause/effect graph: C1 an older ToolReply is retained and its delivery
    // fails after the Runtime has durably staged it; C2 an adjacent System and
    // later Interrupt are committed; C3 a new application instance performs a
    // cold all-batch scan; C4 the later Interrupt succeeds. Effects:
    // E1 C2 is selected only after C1's failed attempt; E2 the exact transferred
    // reply activity is settled, then one root CAS anchors the Interrupt and
    // supersedes the exact reply+System; E3 the original reply error remains
    // observable after that repair; E4 a second cold scan has no damaged
    // backlog, the aggregate is Idle, and a later User is durably admitted.
    //
    // | Rule | Older reply | Later interrupt | Drive | Effects |
    // |---|---|---|---|---|
    // | CR1 | staged then unavailable | succeeds | cold/all batches | E1+E2+E3+E4 |
    //
    // Healthy reply ordering remains covered by P1-P3 above. The boxed fixture
    // isolates the recovery phase from the follow-up phase without changing the
    // production Runtime stack or duplicating either state machine.
    let (repository, cold) = recover_failed_reply_with_interrupt().await;
    let follow_up = cold
        .append_session_event_batch(
            "cold-interrupt-priority",
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("work after recovery")],
            }],
            None,
            None,
        )
        .await
        .expect("CR1/E4 follow-up User is admitted");
    let durable = repository
        .get(&SessionId::from("cold-interrupt-priority"))
        .await
        .unwrap();
    let stored = durable
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == follow_up.batch_id)
        .expect("CR1/E4 admitted User batch is durable");
    assert!(
        matches!(
            &stored.events[0].event,
            SessionEventCommand::UserMessage { .. }
        ),
        "CR1/E4 stale pending reply no longer blocks user.message"
    );
}

#[tokio::test]
async fn cold_recovery_interrupt_failure_preserves_failed_reply_and_activity() {
    // Cause/effect rule CR2: C1 an older ToolReply fails after transferring
    // one exact activity epoch; C2 a later exact-target Interrupt is retained;
    // C3 that Interrupt effect fails. Effects: E1 the Interrupt error wins over
    // the older reply error; E2 reply, adjacent System, and Interrupt remain
    // unprocessed; E3 the transferred activity remains active. This is split
    // from CR1 so each async test owns one decision-table rule and the test
    // harness does not place two complete reconciliation scenarios on one stack.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("CR2 repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.tool_reply_prior_epoch.store(1, Ordering::SeqCst);
    runtime
        .fail_tool_reply_after_stage_once
        .store(true, Ordering::SeqCst);
    runtime
        .fail_primary_interrupt_once
        .store(true, Ordering::SeqCst);
    create(
        repository.as_ref(),
        running_session_with_activity("cold-interrupt-failure", 1),
    )
    .await;
    let warm = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let reply = warm
        .append_session_event_batch(
            "cold-interrupt-failure",
            vec![
                tool_reply_input("older-reply-before-interrupt-failure"),
                SessionEventInput::SystemMessage {
                    content: vec![ContentBlock::text("must remain open")],
                },
            ],
            None,
            None,
        )
        .await
        .expect("CR2 older reply");
    let reply_activity_operation =
        tool_reply_activity_operation_id("cold-interrupt-failure", &reply);
    let interrupt = warm
        .append_session_event_batch(
            "cold-interrupt-failure",
            vec![SessionEventInput::Interrupt(SessionEventInterrupt {
                requested_target: Some(SessionThreadTarget::Primary),
                targets: vec![SessionThreadTarget::Primary],
            })],
            None,
            None,
        )
        .await
        .expect("CR2 later interrupt");
    let cold = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let error = cold
        .drive_session_event_batches(&SessionId::from("cold-interrupt-failure"), None)
        .await
        .expect_err("CR2 Interrupt failure must remain observable");
    assert_eq!(
        error.message, "scripted interruption before primary effect",
        "CR2/E1"
    );
    let durable = repository
        .get(&SessionId::from("cold-interrupt-failure"))
        .await
        .unwrap();
    let reply = durable
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == reply.batch_id)
        .unwrap();
    let interrupt = durable
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == interrupt.batch_id)
        .unwrap();
    assert!(reply.events.iter().all(|entry| !entry.processed), "CR2/E2");
    assert!(!interrupt.events[0].processed, "CR2/E2");
    let (activity_root, reply_activity_epoch) = cold
        .recover_activity_for_operation(
            &SessionId::from("cold-interrupt-failure"),
            &reply_activity_operation,
        )
        .await
        .expect("CR2/E3 exact activity receipt read")
        .expect("CR2/E3 failed delivery transferred activity");
    assert!(
        activity_root
            .active_activity_epochs
            .contains(&reply_activity_epoch),
        "CR2/E3 failed Interrupt cannot settle the reply activity"
    );
}

#[tokio::test]
async fn cold_recovery_interrupt_must_include_the_failed_reply_target() {
    // Cause/effect graph: C1 a failed ToolReply owns exact child Thread T; C2 a
    // later Interrupt targets only primary, or a mixed frozen set including T.
    // Effects: E1 an unrelated Interrupt is neither executed nor used to close
    // reply/System; E2 the original reply error remains observable; E3 a mixed
    // target set containing T is eligible after the reply fails.
    //
    // | Rule | Failed reply target | Later Interrupt targets | Effects |
    // |---|---|---|---|
    // | TF1 | child T | primary only | E1+E2 |
    // | TF2 | primary | child T + primary | E3 (covered by CR1 above) |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("TF1 repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.tool_reply_prior_epoch.store(1, Ordering::SeqCst);
    runtime
        .fail_tool_reply_after_stage_once
        .store(true, Ordering::SeqCst);
    create(
        repository.as_ref(),
        running_session_with_activity("interrupt-target-fence", 1),
    )
    .await;
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let reply = app
        .append_session_event_batch(
            "interrupt-target-fence",
            vec![
                tool_reply_input_for_target(
                    "unrelated-child-reply",
                    SessionThreadTarget::Child(ThreadId("event-child".into())),
                ),
                SessionEventInput::SystemMessage {
                    content: vec![ContentBlock::text("child reply context")],
                },
            ],
            None,
            None,
        )
        .await
        .expect("TF1 reply");
    let interrupt = app
        .append_session_event_batch(
            "interrupt-target-fence",
            vec![SessionEventInput::Interrupt(SessionEventInterrupt {
                requested_target: Some(SessionThreadTarget::Primary),
                targets: vec![SessionThreadTarget::Primary],
            })],
            None,
            None,
        )
        .await
        .expect("TF1 unrelated Interrupt");

    let error = app
        .drive_session_event_batches(&SessionId::from("interrupt-target-fence"), None)
        .await
        .expect_err("TF1/E2 reply failure cannot yield to another Thread's Interrupt");
    assert_eq!(
        error.message, "scripted crash after durable reply stage",
        "TF1/E2"
    );
    assert_eq!(
        runtime.trace.lock().unwrap().as_slice(),
        ["reply:unrelated-child-reply"],
        "TF1/E1"
    );
    assert_eq!(
        runtime.primary_interrupt_calls.load(Ordering::SeqCst),
        0,
        "TF1/E1"
    );
    let durable = repository
        .get(&SessionId::from("interrupt-target-fence"))
        .await
        .unwrap();
    let reply = durable
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == reply.batch_id)
        .unwrap();
    let interrupt = durable
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == interrupt.batch_id)
        .unwrap();
    assert!(reply.events.iter().all(|entry| !entry.processed), "TF1/E1");
    assert!(!interrupt.events[0].processed, "TF1/E1");
}

#[tokio::test]
async fn legacy_processed_interrupt_without_anchor_cannot_starve_later_recovery() {
    // Cause/effect graph: C1 an exact failed ToolReply remains pending; C2 a
    // later matching legacy Interrupt is processed but has no projection
    // anchor; C3 a still-later matching Interrupt is unprocessed and succeeds.
    // Effects: E1 C2 cannot invent an anchor or own supersession; E2 C2 cannot
    // terminate the one retained-order search; E3 C3 settles the exact reply
    // epoch and atomically closes itself plus reply/System.
    //
    // | Rule | Failed reply | First match | Later match | Effects |
    // |---|---|---|---|---|
    // | LA1 | pending/unavailable | processed, no anchor | unprocessed | E1+E2+E3 |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("LA1 repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.tool_reply_prior_epoch.store(1, Ordering::SeqCst);
    create(
        repository.as_ref(),
        running_session_with_activity("legacy-interrupt-anchor", 1),
    )
    .await;
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let reply = app
        .append_session_event_batch(
            "legacy-interrupt-anchor",
            vec![
                tool_reply_input("reply-before-legacy-interrupt"),
                SessionEventInput::SystemMessage {
                    content: vec![ContentBlock::text("legacy interrupt context")],
                },
            ],
            None,
            None,
        )
        .await
        .expect("LA1 reply");
    let reply_activity_operation =
        tool_reply_activity_operation_id("legacy-interrupt-anchor", &reply);
    let legacy_interrupt = app
        .append_session_event_batch(
            "legacy-interrupt-anchor",
            vec![SessionEventInput::Interrupt(SessionEventInterrupt {
                requested_target: Some(SessionThreadTarget::Primary),
                targets: vec![SessionThreadTarget::Primary],
            })],
            None,
            None,
        )
        .await
        .expect("LA1 legacy Interrupt");
    let valid_interrupt = app
        .append_session_event_batch(
            "legacy-interrupt-anchor",
            vec![SessionEventInput::Interrupt(SessionEventInterrupt {
                requested_target: Some(SessionThreadTarget::Primary),
                targets: vec![SessionThreadTarget::Primary],
            })],
            None,
            None,
        )
        .await
        .expect("LA1 valid Interrupt");

    let mut legacy_root = repository
        .get(&SessionId::from("legacy-interrupt-anchor"))
        .await
        .unwrap();
    legacy_root
        .event_batches
        .iter_mut()
        .find(|batch| batch.batch_id == legacy_interrupt.batch_id)
        .unwrap()
        .events[0]
        .processed = true;
    let expected_revision = legacy_root.revision;
    let payload = awaken_session_contract::SessionMutationPayload::Replace(legacy_root);
    let payload_hash = payload.stable_hash();
    assert!(matches!(
        repository
            .commit_mutation(
                "workspace",
                awaken_session_contract::SessionMutation {
                    expected_revision,
                    idempotency: awaken_session_contract::IdempotencyRecord {
                        key: "legacy:processed-interrupt-without-anchor".into(),
                        payload_hash,
                    },
                    payload,
                    lifecycle_facts: Vec::new(),
                },
            )
            .await
            .expect("LA1 persist legacy root"),
        awaken_session_contract::SessionMutationResult::Applied { .. }
    ));
    runtime
        .fail_tool_reply_after_stage_once
        .store(true, Ordering::SeqCst);

    let error = app
        .drive_session_event_batches(&SessionId::from("legacy-interrupt-anchor"), None)
        .await
        .expect_err("LA1 preserves the failed reply signal after recovery");
    assert_eq!(error.message, "scripted crash after durable reply stage");
    assert_eq!(
        runtime.primary_interrupt_calls.load(Ordering::SeqCst),
        1,
        "LA1/E2 only the later valid Interrupt executes"
    );
    let repaired = repository
        .get(&SessionId::from("legacy-interrupt-anchor"))
        .await
        .unwrap();
    let reply = repaired
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == reply.batch_id)
        .unwrap();
    let legacy_interrupt = repaired
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == legacy_interrupt.batch_id)
        .unwrap();
    let valid_interrupt = repaired
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == valid_interrupt.batch_id)
        .unwrap();
    assert!(reply.events.iter().all(|entry| entry.processed), "LA1/E3");
    assert!(legacy_interrupt.events[0].processed, "LA1/E1");
    assert!(
        legacy_interrupt.events[0].projection_anchor.is_none(),
        "LA1/E1 legacy provenance is not invented"
    );
    assert!(valid_interrupt.events[0].processed, "LA1/E3");
    assert_eq!(
        [
            reply.events[0].projection_anchor,
            reply.events[1].projection_anchor,
        ],
        [valid_interrupt.events[0].projection_anchor; 2],
        "LA1/E3 later valid anchor owns exact supersession"
    );
    let (activity_root, reply_activity_epoch) = app
        .recover_activity_for_operation(
            &SessionId::from("legacy-interrupt-anchor"),
            &reply_activity_operation,
        )
        .await
        .expect("LA1/E3 exact activity receipt read")
        .expect("LA1/E3 exact reply activity exists");
    assert!(
        !activity_root
            .active_activity_epochs
            .contains(&reply_activity_epoch),
        "LA1/E3 later valid Interrupt settles exact reply activity"
    );
}

#[tokio::test]
async fn cold_recovery_reuses_a_processed_interrupt_anchor_to_close_the_failed_reply() {
    // Cause/effect graph: C1 an exact ToolReply+System remains unprocessed; C2
    // its later Interrupt effect and root marker committed before a crash; C3
    // the reply then fails on a cold scan. Effects: E1 recovery reuses C2's
    // projection anchor without repeating its Runtime effect; E2 recovery
    // settles the exact transferred reply epoch before one root CAS closes the
    // exact reply and adjacent System; E3 the aggregate is Idle and the next
    // scan is settled.
    //
    // | Rule | Reply | Later Interrupt | Recovery | Effects |
    // |---|---|---|---|---|
    // | PA1 | fails/unprocessed | processed+anchored | cold | E1+E2+E3 |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("PA1 repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.tool_reply_prior_epoch.store(1, Ordering::SeqCst);
    create(
        repository.as_ref(),
        running_session_with_activity("processed-interrupt-recovery", 1),
    )
    .await;
    let warm = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let reply = warm
        .append_session_event_batch(
            "processed-interrupt-recovery",
            vec![
                tool_reply_input("reply-before-processed-interrupt"),
                SessionEventInput::SystemMessage {
                    content: vec![ContentBlock::text("crash-window context")],
                },
            ],
            None,
            None,
        )
        .await
        .expect("PA1 reply");
    let reply_activity_operation =
        tool_reply_activity_operation_id("processed-interrupt-recovery", &reply);
    let interrupt = warm
        .append_session_event_batch(
            "processed-interrupt-recovery",
            vec![SessionEventInput::Interrupt(SessionEventInterrupt {
                requested_target: Some(SessionThreadTarget::Primary),
                targets: vec![SessionThreadTarget::Primary],
            })],
            None,
            None,
        )
        .await
        .expect("PA1 Interrupt");
    warm.drive_session_event_batches(
        &SessionId::from("processed-interrupt-recovery"),
        Some(&interrupt.batch_id),
    )
    .await
    .expect("PA1 crash occurs after the request-local Interrupt anchor");
    assert_eq!(runtime.primary_interrupt_calls.load(Ordering::SeqCst), 1);
    runtime
        .fail_tool_reply_after_stage_once
        .store(true, Ordering::SeqCst);

    let cold = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let error = cold
        .drive_session_event_batches(&SessionId::from("processed-interrupt-recovery"), None)
        .await
        .expect_err("PA1 original reply delivery still reports its failure");
    assert_eq!(error.message, "scripted crash after durable reply stage");
    assert_eq!(
        runtime.primary_interrupt_calls.load(Ordering::SeqCst),
        1,
        "PA1/E1 processed Interrupt cannot repeat its Runtime effect"
    );
    let repaired = repository
        .get(&SessionId::from("processed-interrupt-recovery"))
        .await
        .unwrap();
    let reply = repaired
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == reply.batch_id)
        .unwrap();
    let interrupt = repaired
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == interrupt.batch_id)
        .unwrap();
    assert!(reply.events.iter().all(|entry| entry.processed), "PA1/E2");
    assert_eq!(
        [
            reply.events[0].projection_anchor,
            reply.events[1].projection_anchor,
        ],
        [interrupt.events[0].projection_anchor; 2],
        "PA1/E2 reuses the committed Interrupt anchor"
    );
    let (activity_root, reply_activity_epoch) = cold
        .recover_activity_for_operation(
            &SessionId::from("processed-interrupt-recovery"),
            &reply_activity_operation,
        )
        .await
        .expect("PA1/E2 exact activity receipt read")
        .expect("PA1/E2 reply transferred one activity epoch");
    assert!(
        !activity_root
            .active_activity_epochs
            .contains(&reply_activity_epoch),
        "PA1/E2 processed-anchor replay settles the exact reply epoch"
    );
    assert_eq!(
        activity_root.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "PA1/E3 processed-anchor replay cannot leak Running"
    );
    cold.drive_session_event_batches(&SessionId::from("processed-interrupt-recovery"), None)
        .await
        .expect("PA1/E3 second cold scan is settled");
}

#[tokio::test]
async fn failed_interrupt_root_cas_cannot_supersede_the_failed_reply() {
    // Cause/effect graph: C1 an exact ToolReply fails; C2 the later Interrupt
    // effect succeeds; C3 exact activity settlement succeeds; C4 the single
    // root CAS that would anchor Interrupt and supersede reply+System is
    // unavailable; C5 the retained commands retry. Effects: E1 C4's error wins
    // over C1; E2 none of the three root entries is processed; E3 the exact
    // activity is already settled and cannot reopen; E4 C5 safely replays the
    // idempotent Interrupt effect and the same atomic root CAS.
    //
    // | Rule | Reply effect | Interrupt | Settle | Root CAS | Effects |
    // |---|---|---|---|---|---|
    // | CF1 | unavailable | succeeds | succeeds | unavailable | E1+E2+E3 |
    // | CF2 | exact retry unavailable | idempotent | no-op | succeeds | E3+E4 |
    let inner: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("CF1 durable repository"),
    );
    let faulting = Arc::new(FaultingSessionRepository::new(inner.clone()));
    let repository: Arc<dyn ManagedSessionRepository> = faulting.clone();
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.tool_reply_prior_epoch.store(1, Ordering::SeqCst);
    runtime
        .fail_tool_reply_after_stage_once
        .store(true, Ordering::SeqCst);
    create(
        repository.as_ref(),
        running_session_with_activity("interrupt-cas-failure", 1),
    )
    .await;
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let reply = app
        .append_session_event_batch(
            "interrupt-cas-failure",
            vec![
                tool_reply_input("reply-before-interrupt-cas-failure"),
                SessionEventInput::SystemMessage {
                    content: vec![ContentBlock::text("CAS failure context")],
                },
            ],
            None,
            None,
        )
        .await
        .expect("CF1 reply");
    let reply_activity_operation =
        tool_reply_activity_operation_id("interrupt-cas-failure", &reply);
    let interrupt = app
        .append_session_event_batch(
            "interrupt-cas-failure",
            vec![SessionEventInput::Interrupt(SessionEventInterrupt {
                requested_target: Some(SessionThreadTarget::Primary),
                targets: vec![SessionThreadTarget::Primary],
            })],
            None,
            None,
        )
        .await
        .expect("CF1 Interrupt");
    faulting.fail_once("mark-event-processed");

    let error = app
        .drive_session_event_batches(&SessionId::from("interrupt-cas-failure"), None)
        .await
        .expect_err("CF1 root-CAS outage must remain observable");
    assert!(error.message.contains("injected root-CAS outage"), "CF1/E1");
    assert_ne!(
        error.message, "scripted crash after durable reply stage",
        "CF1/E1"
    );
    assert_eq!(
        runtime.primary_interrupt_calls.load(Ordering::SeqCst),
        1,
        "CF1/E4"
    );
    let durable = inner
        .get(&SessionId::from("interrupt-cas-failure"))
        .await
        .unwrap();
    let reply = durable
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == reply.batch_id)
        .unwrap();
    let interrupt = durable
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == interrupt.batch_id)
        .unwrap();
    assert!(reply.events.iter().all(|entry| !entry.processed), "CF1/E2");
    assert!(!interrupt.events[0].processed, "CF1/E2");
    let (activity_root, reply_activity_epoch) = app
        .recover_activity_for_operation(
            &SessionId::from("interrupt-cas-failure"),
            &reply_activity_operation,
        )
        .await
        .expect("CF1/E3 exact activity receipt read")
        .expect("CF1/E3 exact activity was transferred");
    assert!(
        !activity_root
            .active_activity_epochs
            .contains(&reply_activity_epoch),
        "CF1/E3 settlement precedes the failed root marker"
    );
    assert_eq!(
        activity_root.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "CF1/E3 exact retry cannot reopen a settled receipt"
    );

    runtime
        .fail_tool_reply_after_stage_once
        .store(true, Ordering::SeqCst);
    let retry_error = app
        .drive_session_event_batches(&SessionId::from("interrupt-cas-failure"), None)
        .await
        .expect_err("CF2 keeps the exact reply retry signal after repair");
    assert_eq!(
        retry_error.message, "scripted crash after durable reply stage",
        "CF2/E4"
    );
    assert_eq!(
        runtime.primary_interrupt_calls.load(Ordering::SeqCst),
        2,
        "CF2/E4 retained Interrupt is safely replayed"
    );
    assert_eq!(
        runtime.primary_interrupt_effects.lock().unwrap().len(),
        1,
        "CF2/E4 Runtime interruption effect remains idempotent"
    );
    let repaired = inner
        .get(&SessionId::from("interrupt-cas-failure"))
        .await
        .unwrap();
    assert!(
        repaired
            .event_batches
            .iter()
            .flat_map(|batch| &batch.events)
            .all(|entry| entry.processed),
        "CF2/E4 retry completes the one atomic root supersession"
    );
    assert!(
        !repaired
            .active_activity_epochs
            .contains(&reply_activity_epoch),
        "CF2/E3 exact retry does not reopen the settled epoch"
    );
}

#[tokio::test]
async fn failed_reply_activity_settlement_cannot_close_pending_supersession() {
    // Cause/effect graph: C1 ToolReply delivery transfers old epoch A to exact
    // receipt epoch B and then fails; C2 its later exact-target Interrupt
    // succeeds; C3 settling B fails or succeeds; C4 retained commands replay.
    // Effects: E1 a failed C3 wins over the reply error; E2 reply, adjacent
    // System, and Interrupt all remain pending while B remains active; E3 an
    // exact retry reuses B and the idempotent Interrupt effect; E4 only a
    // successful C3 may atomically close all pending entries and leave Idle.
    //
    // | Rule | Reply | Interrupt | Exact B settle | Root supersession | Effects |
    // |---|---|---|---|---|---|
    // | SF1 | unavailable | succeeds | unavailable | forbidden | E1+E2 |
    // | SF2 | exact retry unavailable | idempotent | succeeds | succeeds | E3+E4 |
    let inner: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("SF1 durable repository"),
    );
    let faulting = Arc::new(FaultingSessionRepository::new(inner.clone()));
    let repository: Arc<dyn ManagedSessionRepository> = faulting.clone();
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.tool_reply_prior_epoch.store(1, Ordering::SeqCst);
    runtime
        .fail_tool_reply_after_stage_once
        .store(true, Ordering::SeqCst);
    create(
        repository.as_ref(),
        running_session_with_activity("interrupt-settle-failure", 1),
    )
    .await;
    let app = application_with_runtime(
        runtime.clone(),
        repository,
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let reply = app
        .append_session_event_batch(
            "interrupt-settle-failure",
            vec![
                tool_reply_input("reply-before-settle-failure"),
                SessionEventInput::SystemMessage {
                    content: vec![ContentBlock::text("settlement failure context")],
                },
            ],
            None,
            None,
        )
        .await
        .expect("SF1 reply");
    let reply_activity_operation =
        tool_reply_activity_operation_id("interrupt-settle-failure", &reply);
    let interrupt = app
        .append_session_event_batch(
            "interrupt-settle-failure",
            vec![SessionEventInput::Interrupt(SessionEventInterrupt {
                requested_target: Some(SessionThreadTarget::Primary),
                targets: vec![SessionThreadTarget::Primary],
            })],
            None,
            None,
        )
        .await
        .expect("SF1 Interrupt");
    faulting.fail_once("settle-activity");

    let error = app
        .drive_session_event_batches(&SessionId::from("interrupt-settle-failure"), None)
        .await
        .expect_err("SF1 exact activity settlement outage is observable");
    assert!(error.message.contains("injected root-CAS outage"), "SF1/E1");
    let durable = inner
        .get(&SessionId::from("interrupt-settle-failure"))
        .await
        .unwrap();
    let reply_after_failure = durable
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == reply.batch_id)
        .unwrap();
    let interrupt_after_failure = durable
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == interrupt.batch_id)
        .unwrap();
    assert!(
        reply_after_failure
            .events
            .iter()
            .all(|entry| !entry.processed),
        "SF1/E2"
    );
    assert!(!interrupt_after_failure.events[0].processed, "SF1/E2");
    let (activity_root, reply_activity_epoch) = app
        .recover_activity_for_operation(
            &SessionId::from("interrupt-settle-failure"),
            &reply_activity_operation,
        )
        .await
        .expect("SF1/E2 exact activity receipt read")
        .expect("SF1/E2 exact reply activity exists");
    assert_ne!(reply_activity_epoch, 1, "SF1/E2 exact transferred epoch");
    assert!(
        activity_root
            .active_activity_epochs
            .contains(&reply_activity_epoch),
        "SF1/E2 failed settlement retains exact activity"
    );

    runtime
        .fail_tool_reply_after_stage_once
        .store(true, Ordering::SeqCst);
    let retry_error = app
        .drive_session_event_batches(&SessionId::from("interrupt-settle-failure"), None)
        .await
        .expect_err("SF2 original reply error remains after repair");
    assert_eq!(
        retry_error.message, "scripted crash after durable reply stage",
        "SF2/E3"
    );
    assert_eq!(
        runtime.primary_interrupt_calls.load(Ordering::SeqCst),
        2,
        "SF2/E3 retained Interrupt replayed"
    );
    assert_eq!(
        runtime.primary_interrupt_effects.lock().unwrap().len(),
        1,
        "SF2/E3 Interrupt effect is idempotent"
    );
    let repaired = inner
        .get(&SessionId::from("interrupt-settle-failure"))
        .await
        .unwrap();
    assert!(
        repaired
            .event_batches
            .iter()
            .flat_map(|batch| &batch.events)
            .all(|entry| entry.processed),
        "SF2/E4 exact settlement permits atomic root closure"
    );
    assert!(
        !repaired
            .active_activity_epochs
            .contains(&reply_activity_epoch),
        "SF2/E4 exact activity settled"
    );
    assert_eq!(
        repaired.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "SF2/E4 aggregate closes only after exact settlement"
    );
}

#[tokio::test]
async fn preferred_interrupt_does_not_reenter_an_older_active_outcome() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect: C1 an older DefineOutcome entry is still unprocessed while
    // its Thread aggregate is already active; C2 a later HTTP batch durably
    // admits an exact primary Interrupt. E1 the request-local drive selects C2
    // without entering C1's serialized prepare/execute owner; E2 C2 is marked
    // processed; E3 C1 remains retained for the lifecycle supervisor. Re-driving
    // C1 would make interrupt admission wait until the Outcome was terminal.
    //
    // | Rule | older Outcome | preferred batch | Effects |
    // |---|---|---|---|
    // | OI1 | active/unprocessed | Interrupt | E1 + E2 + E3 |
    // | OI2 | active/unprocessed | User only | no request-local effect |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.outcome_busy.store(true, Ordering::SeqCst);
    create(
        repository.as_ref(),
        persisted("outcome-interrupt-priority", false, "running"),
    )
    .await;
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let outcome = app
        .append_session_event_batch(
            "outcome-interrupt-priority",
            vec![SessionEventInput::DefineOutcome {
                description: "ship".into(),
                rubric: SessionOutcomeRubric::Text {
                    content: "correct".into(),
                },
                max_iterations: Some(2),
            }],
            None,
            None,
        )
        .await
        .expect("OI1 older Outcome");
    let interrupt = app
        .append_session_event_batch(
            "outcome-interrupt-priority",
            vec![SessionEventInput::Interrupt(SessionEventInterrupt {
                requested_target: Some(SessionThreadTarget::Primary),
                targets: vec![SessionThreadTarget::Primary],
            })],
            None,
            None,
        )
        .await
        .expect("OI1 preferred Interrupt");

    app.drive_session_event_batches(
        &SessionId::from("outcome-interrupt-priority"),
        Some(&interrupt.batch_id),
    )
    .await
    .expect("OI1 request-local drive");

    assert!(
        runtime.outcome_prepare_calls.lock().unwrap().is_empty(),
        "OI1/E1"
    );
    assert_eq!(
        runtime.trace.lock().unwrap().as_slice(),
        ["interrupt:primary"],
        "OI1/E1"
    );
    let persisted = repository
        .get(&SessionId::from("outcome-interrupt-priority"))
        .await
        .unwrap();
    assert!(
        persisted
            .event_batches
            .iter()
            .find(|batch| batch.batch_id == interrupt.batch_id)
            .unwrap()
            .events[0]
            .processed,
        "OI1/E2"
    );
    assert!(
        !persisted
            .event_batches
            .iter()
            .find(|batch| batch.batch_id == outcome.batch_id)
            .unwrap()
            .events[0]
            .processed,
        "OI1/E3"
    );
}

#[tokio::test]
async fn reply_stage_response_loss_replays_one_durable_effect_after_restart() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 exact ToolReply is unprocessed; C2 Runtime durably
    // stages it but its response is lost before root processed CAS; C3 a cold
    // application retries the same retained command; C4 a later scan sees the
    // processed marker; C5 admission froze Awaiting cursor 777. Effects: E1 first
    // drive reports retryable failure and
    // leaves root unprocessed; E2 the one coordination/activity identity is
    // replayed; E3 Runtime owns one staged payload despite two delivery calls;
    // E4 cold retry classifies the durable receipt before the consumed-ticket
    // path, marks processed at C5, and later scans perform no delivery.
    //
    // | Rule | Stage | Root marker | Drive | Effect |
    // | R1 | new success/response lost | false | warm | E1+E3 |
    // | R2 | exact replay | false | cold | E2+E3+E4 |
    // | R3 | retained | true | any | E4 skip |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.tool_reply_prior_epoch.store(1, Ordering::SeqCst);
    runtime
        .fail_tool_reply_after_stage_once
        .store(true, Ordering::SeqCst);
    create(
        repository.as_ref(),
        running_session_with_activity("reply-replay", 1),
    )
    .await;
    let warm = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    warm.append_session_event_batch(
        "reply-replay",
        vec![tool_reply_input("reply-replay-tool")],
        None,
        None,
    )
    .await
    .expect("R1 append");

    let interrupted = warm
        .drive_session_event_batches(&SessionId::from("reply-replay"), None)
        .await;
    assert!(
        interrupted
            .is_err_and(|error| error.kind == awaken_session_contract::RunErrorKind::Unavailable),
        "R1/E1"
    );
    assert!(
        !repository
            .get(&SessionId::from("reply-replay"))
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "R1/E1"
    );
    assert_eq!(
        runtime.staged_tool_replies.lock().unwrap().len(),
        1,
        "R1/E3"
    );

    let cold = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    cold.drive_session_event_batches(&SessionId::from("reply-replay"), None)
        .await
        .expect("R2 cold replay");
    assert_eq!(runtime.tool_reply_calls.load(Ordering::SeqCst), 2, "R2/E2");
    assert_eq!(
        runtime.staged_tool_replies.lock().unwrap().len(),
        1,
        "R2/E3"
    );
    assert!(
        repository
            .get(&SessionId::from("reply-replay"))
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "R2/E4"
    );
    assert_eq!(
        repository
            .get(&SessionId::from("reply-replay"))
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .projection_anchor
            .map(|anchor| anchor.source_commit_cursor),
        Some(777),
        "R2/C5 freezes the answered Awaiting boundary across response loss"
    );
    cold.drive_session_event_batches(&SessionId::from("reply-replay"), None)
        .await
        .expect("R3 replay skip");
    assert_eq!(runtime.tool_reply_calls.load(Ordering::SeqCst), 2, "R3/E4");
}

#[tokio::test]
async fn interrupt_frozen_targets_recover_idempotently_after_partial_failure() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 accepted interrupt freezes Primary+child; C2
    // one target fails before effect; C3 the failed target is before or after a
    // healthy target in the frozen order; C4 process restarts; C5 the retained
    // marker is false/true. Effects: E1 no partial attempt may mark the root
    // processed; E2 every target is attempted even when an earlier target fails;
    // E3 retry reuses the exact frozen set; E4 idempotent interrupt owners expose
    // one effect per target despite repeated calls; E5 only the all-success retry
    // marks processed and later scans skip it.
    //
    // | Rule | Primary | Child | Marker | Effect |
    // | I1 | success | unavailable | false | E1+E2 |
    // | I2 | exact replay | success | false | E3+E4+E5 mark |
    // | I3 | retained | retained | true | E5 skip |
    // | I4 | unavailable | success | false | E1+E2, then I2 |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime
        .fail_child_interrupt_once
        .store(true, Ordering::SeqCst);
    create(
        repository.as_ref(),
        persisted("interrupt-replay", false, "idle"),
    )
    .await;
    let warm = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    warm.append_session_event_batch(
        "interrupt-replay",
        vec![SessionEventInput::Interrupt(SessionEventInterrupt {
            requested_target: None,
            targets: vec![
                SessionThreadTarget::Primary,
                SessionThreadTarget::Child(ThreadId("event-child".into())),
            ],
        })],
        None,
        None,
    )
    .await
    .expect("I1 append");

    assert!(
        warm.drive_session_event_batches(&SessionId::from("interrupt-replay"), None)
            .await
            .is_err(),
        "I1/E1"
    );
    assert!(
        !repository
            .get(&SessionId::from("interrupt-replay"))
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "I1/E1"
    );
    let cold = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    cold.drive_session_event_batches(&SessionId::from("interrupt-replay"), None)
        .await
        .expect("I2 recovery");
    assert_eq!(
        runtime.primary_interrupt_calls.load(Ordering::SeqCst),
        2,
        "I2/E2"
    );
    assert_eq!(
        runtime.child_interrupt_calls.load(Ordering::SeqCst),
        2,
        "I2/E2"
    );
    assert_eq!(
        runtime
            .primary_interrupt_effects
            .lock()
            .unwrap()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["interrupt-replay"],
        "I2/E3"
    );
    assert_eq!(
        runtime.child_interrupt_effects.lock().unwrap().len(),
        1,
        "I2/E3"
    );
    assert!(
        runtime
            .child_interrupt_effects
            .lock()
            .unwrap()
            .contains(&("interrupt-replay".into(), "event-child".into())),
        "I2/E2+E3"
    );
    assert!(
        repository
            .get(&SessionId::from("interrupt-replay"))
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "I2/E4"
    );
    cold.drive_session_event_batches(&SessionId::from("interrupt-replay"), None)
        .await
        .expect("I3 processed skip");
    assert_eq!(
        runtime.primary_interrupt_calls.load(Ordering::SeqCst),
        2,
        "I3/E4"
    );
    assert_eq!(
        runtime.child_interrupt_calls.load(Ordering::SeqCst),
        2,
        "I3/E4"
    );

    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("primary-failure repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime
        .fail_primary_interrupt_once
        .store(true, Ordering::SeqCst);
    create(
        repository.as_ref(),
        persisted("interrupt-primary-replay", false, "idle"),
    )
    .await;
    let warm = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    warm.append_session_event_batch(
        "interrupt-primary-replay",
        vec![SessionEventInput::Interrupt(SessionEventInterrupt {
            requested_target: None,
            targets: vec![
                SessionThreadTarget::Primary,
                SessionThreadTarget::Child(ThreadId("event-child".into())),
            ],
        })],
        None,
        None,
    )
    .await
    .expect("I4 append");
    assert!(
        warm.drive_session_event_batches(&SessionId::from("interrupt-primary-replay"), None)
            .await
            .is_err(),
        "I4/E1"
    );
    assert_eq!(
        runtime.primary_interrupt_calls.load(Ordering::SeqCst),
        1,
        "I4/E2 primary attempted"
    );
    assert_eq!(
        runtime.child_interrupt_calls.load(Ordering::SeqCst),
        1,
        "I4/E2 later child still attempted"
    );
    assert!(
        runtime.primary_interrupt_effects.lock().unwrap().is_empty(),
        "I4/E1 failed target has no effect"
    );
    assert_eq!(
        runtime.child_interrupt_effects.lock().unwrap().len(),
        1,
        "I4/E2 healthy later target takes effect"
    );
    assert!(
        !repository
            .get(&SessionId::from("interrupt-primary-replay"))
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "I4/E1"
    );

    let cold = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    cold.drive_session_event_batches(&SessionId::from("interrupt-primary-replay"), None)
        .await
        .expect("I4 then I2 recovery");
    assert_eq!(
        runtime.primary_interrupt_calls.load(Ordering::SeqCst),
        2,
        "I4->I2/E3"
    );
    assert_eq!(
        runtime.child_interrupt_calls.load(Ordering::SeqCst),
        2,
        "I4->I2/E3"
    );
    assert_eq!(
        runtime.primary_interrupt_effects.lock().unwrap().len(),
        1,
        "I4->I2/E4"
    );
    assert_eq!(
        runtime.child_interrupt_effects.lock().unwrap().len(),
        1,
        "I4->I2/E4"
    );
    assert!(
        repository
            .get(&SessionId::from("interrupt-primary-replay"))
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "I4->I2/E5"
    );
}

#[test]
fn user_system_batch_recovers_through_one_dispatch_and_thread_truth() {
    // Coverage rationale. Causes: the canonical helper below partitions absent,
    // Reserved, active, and Ended User truth across restart/active-active scans.
    // Effects: it proves one dispatch plus ordered System/User committed truth.
    // Constraint/Invariant: this wrapper owns no second scenario or oracle.
    // Decision rule: delegate once to the helper's R1-R4 recovery matrix.
    // Keep the one R1-R4 helper and oracle unchanged; the shared test executor
    // only moves its large composed future off the default libtest stack.
    run_composed_async_test(user_system_batch_case);
}

async fn user_system_batch_case() {
    // Crash/active-active cause-effect graph: C1 User Run absent/reserved/
    // active/Ended; C2 trailing System absent/present in the User reservation/
    // committed; C3 one or two supervisors scan the same root; C4 process
    // restarts between any boundaries. Effects E1 one effective reservation and
    // activation per stable Run; E2 one reservation freezes System before User;
    // E3 retained entries wait for authoritative Run/Message truth; E4 cold replay
    // resumes and settles the original batch epoch without duplicate effects;
    // E5 Running is retained between User entries and Idle appears only after
    // the final User Dispatch atomically settles its paired System input.
    //
    // | Rule | User truth | System truth | Scanners | Effect |
    // | R1 | absent | none | one | reserve -> activity -> activate |
    // | R2 | Ended first User | root intent | one | reserve [System,User], activate |
    // | R3 | Running | committed | two/restart | wait, no second effects |
    // | R4 | Ended target | committed | cold | advance both, settle, Idle |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    let session = planned_session(
        "initial-user-system",
        vec![
            SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("first")],
            },
            SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("accompanying")],
            },
            SessionEventInput::SystemMessage {
                content: vec![ContentBlock::text("durable context")],
            },
        ],
    );
    let batch_epoch = session
        .event_batches
        .first()
        .expect("batch")
        .wake_activity_epoch
        .expect("create wake");
    let batch = session.event_batches.first().expect("batch");
    let run_ids = batch
        .events
        .iter()
        .filter_map(|entry| match &entry.event {
            awaken_session_contract::SessionEventCommand::UserMessage { run_id, .. } => {
                Some(run_id.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let (system_operation, system_content) = match &batch.events[2].event {
        awaken_session_contract::SessionEventCommand::SystemMessage {
            operation_id,
            content,
        } => (operation_id.clone(), content.clone()),
        _ => panic!("R2 System intent"),
    };
    create(repository.as_ref(), session).await;
    let app = Arc::new(application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));

    let first = app.reconcile_event_batches().await;
    assert!(first.failures.is_empty(), "R1: {:?}", first.failures);
    assert_eq!(runtime.reserved.lock().unwrap().len(), 1, "R1/E1");
    runtime.end(&run_ids[0]);
    finalize_applied_user_dispatch(&app, &runtime, "initial-user-system", &run_ids[0]).await;
    let partially_settled = repository
        .get(&SessionId::from("initial-user-system"))
        .await
        .unwrap();
    assert_eq!(
        partially_settled.execution,
        awaken_session_contract::SessionExecutionState::Running,
        "R1/E4 initial execution cannot become Idle while later Events remain"
    );
    assert!(partially_settled.event_batches[0].events[0].processed);
    assert!(!partially_settled.event_batches[0].events[1].processed);

    let second = app.reconcile_event_batches().await;
    assert!(second.failures.is_empty(), "R2: {:?}", second.failures);
    assert_eq!(runtime.reserved.lock().unwrap().len(), 2, "R2/E1");
    let target_command = runtime
        .reserved
        .lock()
        .unwrap()
        .get(&run_ids[1].0)
        .cloned()
        .expect("R2/E2 target reservation");
    let expected_system = Message::new(
        MessageId::session_system("initial-user-system", &system_operation),
        Role::System,
        system_content,
    );
    assert_eq!(
        target_command.messages.first(),
        Some(&expected_system),
        "R2/E2 one complete neutral reservation"
    );
    let trace = runtime.trace.lock().unwrap().clone();
    let target_reservation = trace
        .iter()
        .position(|entry| entry == &format!("reserve:{}", run_ids[1].0))
        .expect("R2/E2 target reserved");
    let target_activation = trace
        .iter()
        .position(|entry| entry == &format!("activate:{}", run_ids[1].0))
        .expect("R2/E2 target activated");
    assert!(target_reservation < target_activation, "R2/E2");
    let committed = runtime.committed.lock().unwrap().clone();
    let system_id = MessageId::session_system("initial-user-system", &system_operation);
    let user_id =
        MessageId::session_event_input("initial-user-system", &target_command.operation_id);
    let system_position = committed
        .iter()
        .position(|message| message.id == system_id)
        .expect("R2/E2 committed System");
    let user_position = committed
        .iter()
        .position(|message| message.id == user_id)
        .expect("R2/E2 committed User");
    assert!(system_position < user_position, "R2/E2 System before User");

    let restarted = Arc::new(application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let warm_app = Arc::clone(&app);
    let cold_app = Arc::clone(&restarted);
    let warm_task = tokio::spawn(async move { warm_app.reconcile_event_batches().await });
    let cold_task = tokio::spawn(async move { cold_app.reconcile_event_batches().await });
    let warm = warm_task.await.expect("R3 warm supervisor");
    let cold = cold_task.await.expect("R3 cold supervisor");
    assert!(
        warm.failures.is_empty() && cold.failures.is_empty(),
        "R3/E3"
    );
    assert_eq!(runtime.reserved.lock().unwrap().len(), 2, "R3/E1");
    assert_eq!(runtime.activated.lock().unwrap().len(), 2, "R3/E1");

    runtime.end(&run_ids[1]);
    finalize_applied_user_dispatch(&restarted, &runtime, "initial-user-system", &run_ids[1]).await;
    let completed = restarted.reconcile_event_batches().await;
    assert!(completed.failures.is_empty(), "R4/E4");
    assert_eq!(
        completed.settled, 0,
        "R4/E4 post-Dispatch aggregate CAS already completed the paired System input"
    );
    let durable = repository
        .get(&SessionId::from("initial-user-system"))
        .await
        .expect("R4 durable root");
    let batch = durable.event_batches.first().expect("R4 batch");
    assert!(batch.is_complete(), "R4/E3");
    assert_eq!(
        durable.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "R4/E4 only complete Event and activity settlement reaches Idle"
    );
    assert!(
        !durable.active_activity_epochs.contains(&batch_epoch),
        "R4/E4"
    );
    assert_eq!(runtime.reserved.lock().unwrap().len(), 2, "R4/E1");
    assert_eq!(runtime.activated.lock().unwrap().len(), 2, "R4/E1");
}

#[tokio::test]
async fn fresh_user_reservation_snapshots_the_recovered_resource_generation() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Projection/reservation cause-effect graph: C1 no activity receipt exists;
    // C2 the aggregate has amended an unattempted pending Resource generation;
    // C3 the process-local Host still carries the prior manifest at that same
    // revision. Effects: E1 admission replaces the disposable projection from
    // aggregate truth before reservation; E2 the immutable Run snapshots the
    // amended manifest; E3 the Worker never receives one revision with two
    // values, while the generation fence remains strict.
    //
    // | Rule | Receipt | Durable desired | Host projection | Effect |
    // |---|---|---|---|---|
    // | G1 | absent | rev1/file-new | rev1/file-old | E1 -> E2 -> E3 |
    // | G2 | existing | any later policy | any | receipt replay; covered by `exact_activity_receipt_precedes_fresh_budget_policy` |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("generation-order repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    let prior = file_resources("file-old");
    let desired = file_resources("file-new");
    let mut session = persisted("generation-before-reservation", true, "preparing");
    let revision = session
        .resources
        .prepare(session.session_id.as_str(), prior.clone())
        .expect("G1 pending generation");
    assert_eq!(
        session
            .resources
            .revise_unattempted_pending(session.session_id.as_str(), desired.clone())
            .expect("G1 amendment"),
        revision,
        "G1 same generation"
    );
    *runtime.projected_resources.lock().unwrap() = Some((revision, prior));
    create(repository.as_ref(), session).await;
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let run_id = RunId("generation-run".into());

    let admission = app
        .admit_session_user_run(SessionUserRunCommand {
            session_id: "generation-before-reservation".into(),
            agent_id: "agent".into(),
            operation_id: "generation-operation".into(),
            run_id: run_id.clone(),
            content: vec![ContentBlock::text("drive the amended generation")],
            accompanying_system: None,
            data_subject_id: None,
            traceparent: None,
        })
        .await
        .expect("G1 fresh admission");

    assert!(
        matches!(admission, AdmittedSessionRun::Reserved(_)),
        "G1/E2"
    );
    assert_eq!(
        runtime
            .reserved_resource_generations
            .lock()
            .unwrap()
            .get(&run_id.0),
        Some(&(revision, desired)),
        "G1/E2 reservation freezes aggregate desired truth"
    );
    let trace = runtime.trace.lock().unwrap();
    let projection = trace
        .iter()
        .position(|entry| entry == &format!("project:generation-before-reservation:{revision}"))
        .expect("G1/E1 projection");
    let reservation = trace
        .iter()
        .position(|entry| entry == "reserve:generation-run")
        .expect("G1/E2 reservation");
    assert!(projection < reservation, "G1/E1 before E2");
}

#[tokio::test]
async fn reserved_run_without_activity_recovers_through_the_same_operation_receipt() {
    // Causes: C1-C4 cover projection recovery, reservation, crash before activity,
    // and cold exact retry.
    // Reserve/activity crash graph: C1 canonical projection recovery succeeds;
    // C2 the immutable reservation commits; C3 the process stops before the
    // activity CAS; C4 a cold exact retry sees no receipt plus AlreadyReserved.
    // Effects: E1 no execution is published at C3; E2 C4 recovers projection,
    // reuses the reservation, and commits one operation->epoch receipt; E3 no
    // second Run identity or activity is created.
    //
    // | Rule | Projection | Reservation | Receipt | Effect |
    // |---|---|---|---|---|
    // | C1 | recovered | absent -> Reserved | absent (crash) | E1 |
    // | C2 | recovered | AlreadyReserved | absent -> committed | E2+E3 |
    // Constraint/Invariant: reservation and operation receipt reuse one Run and
    // root activity authority. Decision rule: execute crash rule C1 followed by
    // cold recovery C2 and require E1-E3.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("reservation-repair repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    create(
        repository.as_ref(),
        persisted("reservation-before-activity", false, "idle"),
    )
    .await;
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let warm = application_with_runtime(runtime.clone(), repository.clone(), environments.clone());
    let command = SessionUserRunCommand {
        session_id: "reservation-before-activity".into(),
        agent_id: "agent".into(),
        operation_id: "reservation-operation".into(),
        run_id: RunId("reservation-run".into()),
        content: vec![ContentBlock::text("resume after reservation")],
        accompanying_system: None,
        data_subject_id: None,
        traceparent: None,
    };
    warm.admit_run_session("workspace", "reservation-before-activity", "agent")
        .await
        .expect("C1 canonical projection");
    assert_eq!(
        runtime
            .reserve_session_run(AdmitSessionRun {
                session_id: command.session_id.clone(),
                agent_id: command.agent_id.clone(),
                operation_id: command.operation_id.clone(),
                run_id: command.run_id.clone(),
                messages: vec![Message::new(
                    MessageId::session_event_input(
                        command.session_id.as_str(),
                        &command.operation_id
                    ),
                    Role::User,
                    command.content.clone(),
                )],
                data_subject_id: command.data_subject_id.clone(),
                traceparent: command.traceparent.clone(),
                execution_requirements: Default::default(),
                replacement: awaken_session_contract::SessionRunReplacement::PreservePrior,
            })
            .await
            .expect("C1 reservation"),
        SessionRunReservation::Reserved,
        "C1/E1"
    );
    let operation = awaken_session_contract::session_run_activity_operation_id(
        "reservation-before-activity",
        &command.run_id,
    );
    assert!(
        warm.recover_activity_for_operation(
            &SessionId::from("reservation-before-activity"),
            &operation
        )
        .await
        .expect("C1 receipt query")
        .is_none(),
        "C1/E1 no activity before retry"
    );

    let cold = application_with_runtime(runtime.clone(), repository.clone(), environments);
    let repaired = cold
        .admit_session_user_run(command)
        .await
        .expect("C2 repair");
    let epoch = repaired
        .delivery()
        .expect("C2/E2 delivery")
        .session_activity_epoch;
    assert!(
        matches!(repaired, AdmittedSessionRun::AlreadyReserved(_)),
        "C2/E2"
    );
    assert_eq!(runtime.reserved.lock().unwrap().len(), 1, "C2/E3");
    assert_eq!(
        cold.recover_activity_for_operation(
            &SessionId::from("reservation-before-activity"),
            &operation
        )
        .await
        .expect("C2 receipt query")
        .expect("C2/E2 receipt")
        .1,
        epoch,
        "C2/E2 exact operation receipt"
    );
}

#[tokio::test]
async fn exact_activity_receipt_precedes_fresh_budget_policy() {
    // Causes: C1-C3 cover committed reservation/receipt, lost response, and a
    // newly reached budget at exact retry.
    // Receipt/policy replay graph: C1 a reservation and activity receipt both
    // committed; C2 the response was lost; C3 the Session budget is reached
    // before exact retry. Effects: E1 retry does not evaluate fresh projection
    // or activity policy; E2 Runtime classifies the existing reservation; E3
    // the original epoch is returned with no Session root mutation.
    //
    // | Rule | Receipt | Current budget | Reservation truth | Effect |
    // |---|---|---|---|---|
    // | P1 | absent | open | absent | fresh recover -> reserve -> activity |
    // | P2 | exact | reached | Reserved | E1+E2+E3 |
    // Constraint/Invariant: an exact committed receipt dominates fresh budget
    // policy and cannot mutate the Session root. Decision rule: use P1 as setup,
    // then execute P2 after budget closure and require E1-E3.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("receipt-policy repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    let snapshot = awaken_session_contract::ManagedListPriceSnapshot {
        snapshot_id: "receipt-policy-price-v1".into(),
        version: 1,
        effective_at_unix_ms: 1,
        arithmetic_version: 1,
        model_rates: BTreeMap::from([(
            "model".into(),
            awaken_session_contract::ManagedTokenListRates {
                input_micros_per_million: 1_000_000,
                ..Default::default()
            },
        )]),
        runtime_rates: Default::default(),
        fingerprint: "receipt-policy-price-v1-fingerprint".into(),
    };
    let mut session = persisted("receipt-before-policy", false, "idle");
    session.budget = awaken_session_contract::SessionBudgetState::active(1, snapshot);
    create(repository.as_ref(), session).await;
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let command = SessionUserRunCommand {
        session_id: "receipt-before-policy".into(),
        agent_id: "agent".into(),
        operation_id: "receipt-operation".into(),
        run_id: RunId("receipt-run".into()),
        content: vec![ContentBlock::text("one admitted command")],
        accompanying_system: None,
        data_subject_id: None,
        traceparent: None,
    };
    let first = app
        .admit_session_user_run(command.clone())
        .await
        .expect("P1 fresh admission");
    let first_epoch = first
        .delivery()
        .expect("P1 delivery")
        .session_activity_epoch;
    app.reconcile_managed_budget_usage(
        &SessionId::from("receipt-before-policy"),
        awaken_session_contract::SessionUsage {
            input_tokens: 10_000,
            by_model: BTreeMap::from([(
                "model".into(),
                awaken_session_contract::SessionModelUsage {
                    input_tokens: 10_000,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        },
    )
    .await
    .expect("P2 budget settlement");
    let before = repository
        .get(&SessionId::from("receipt-before-policy"))
        .await
        .unwrap()
        .revision;
    let projections_before = runtime
        .trace
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.starts_with("project:"))
        .count();

    let replay = app
        .admit_session_user_run(command)
        .await
        .expect("P2 exact response-loss replay");
    assert!(
        matches!(replay, AdmittedSessionRun::AlreadyReserved(_)),
        "P2/E2"
    );
    assert_eq!(
        replay
            .delivery()
            .expect("P2/E3 delivery")
            .session_activity_epoch,
        first_epoch,
        "P2/E3 original epoch"
    );
    assert_eq!(
        repository
            .get(&SessionId::from("receipt-before-policy"))
            .await
            .unwrap()
            .revision,
        before,
        "P2/E3 no root CAS"
    );
    assert_eq!(
        runtime
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.starts_with("project:"))
            .count(),
        projections_before,
        "P2/E1 no fresh projection policy"
    );
}

#[tokio::test]
async fn user_run_admission_preserves_typed_recovery_and_one_activity_receipt() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 reservation is new/exact replay/already activated/
    // recovery-claimed/completed; C2 the stable Run operation is same/different.
    // Effects: E1 Reserved and AlreadyReserved carry the same exact activity
    // epoch; E2 AlreadyActivated carries the dispatch-owned epoch without a new
    // Session mutation; E3 RecoveryClaimed and Completed retain only Session/Run
    // observation identity and never manufacture a delivery or activity CAS.
    //
    // | Rule | Reservation | Coordinate | Effect |
    // | A1 | Reserved | op-a | E1 delivery(epoch-a) |
    // | A2 | AlreadyReserved | op-a | E1 same delivery(epoch-a) |
    // | A3 | AlreadyActivated(71) | op-a | E2 delivery(71), no root CAS |
    // | A4 | RecoveryClaimed | op-b | E3 identity-only, no root CAS |
    // | A5 | Completed | op-c | E3 identity-only, no root CAS |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    create(
        repository.as_ref(),
        planned_session(
            "user-run-admission",
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("root")],
            }],
        ),
    )
    .await;
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let command = |operation_id: &str, run_id: &str| SessionUserRunCommand {
        session_id: "user-run-admission".into(),
        agent_id: "agent".into(),
        operation_id: operation_id.into(),
        run_id: RunId(run_id.into()),
        content: vec![ContentBlock::text(operation_id)],
        accompanying_system: None,
        data_subject_id: None,
        traceparent: None,
    };

    let first = app
        .admit_session_user_run(command("op-a", "run-a"))
        .await
        .expect("A1");
    let first_epoch = first
        .delivery()
        .expect("A1/E1 delivery")
        .session_activity_epoch;
    assert!(matches!(first, AdmittedSessionRun::Reserved(_)), "A1");

    let replay = app
        .admit_session_user_run(command("op-a", "run-a"))
        .await
        .expect("A2");
    assert!(
        matches!(replay, AdmittedSessionRun::AlreadyReserved(_)),
        "A2"
    );
    assert_eq!(
        replay
            .delivery()
            .expect("A2/E1 delivery")
            .session_activity_epoch,
        first_epoch,
        "A2/E1 exact activity replay"
    );

    runtime.reservation_outcomes.lock().unwrap().extend([
        SessionRunReservation::AlreadyActivated {
            session_activity_epoch: 71,
        },
        SessionRunReservation::RecoveryClaimed,
        SessionRunReservation::Completed,
    ]);
    let before = repository
        .get(&SessionId::from("user-run-admission"))
        .await
        .unwrap()
        .revision;
    let activated = app
        .admit_session_user_run(command("op-a", "run-a"))
        .await
        .expect("A3");
    assert!(
        matches!(activated, AdmittedSessionRun::AlreadyActivated(_)),
        "A3"
    );
    assert_eq!(
        activated
            .delivery()
            .expect("A3/E2 delivery")
            .session_activity_epoch,
        71,
        "A3/E2"
    );
    assert_eq!(
        repository
            .get(&SessionId::from("user-run-admission"))
            .await
            .unwrap()
            .revision,
        before,
        "A3/E2 no root CAS"
    );

    let before_recovery = repository
        .get(&SessionId::from("user-run-admission"))
        .await
        .unwrap()
        .revision;
    let recovery = app
        .admit_session_user_run(command("op-b", "run-b"))
        .await
        .expect("A4");
    assert!(
        matches!(
            recovery,
            AdmittedSessionRun::RecoveryClaimed { ref session_id, ref run_id }
                if session_id == "user-run-admission" && run_id.0 == "run-b"
        ),
        "A4/E3"
    );
    assert!(recovery.delivery().is_none(), "A4/E3");
    assert_eq!(
        repository
            .get(&SessionId::from("user-run-admission"))
            .await
            .unwrap()
            .revision,
        before_recovery,
        "A4/E3 no root CAS"
    );

    let before_completed = repository
        .get(&SessionId::from("user-run-admission"))
        .await
        .unwrap()
        .revision;
    let completed = app
        .admit_session_user_run(command("op-c", "run-c"))
        .await
        .expect("A5");
    assert!(
        matches!(
            completed,
            AdmittedSessionRun::Completed { ref session_id, ref run_id }
                if session_id == "user-run-admission" && run_id.0 == "run-c"
        ),
        "A5/E3"
    );
    assert!(completed.delivery().is_none(), "A5/E3");
    assert_eq!(
        repository
            .get(&SessionId::from("user-run-admission"))
            .await
            .unwrap()
            .revision,
        before_completed,
        "A5/E3 no root CAS"
    );
}

#[tokio::test]
async fn user_run_activity_rejects_a_root_changed_after_projection_recovery() {
    // Cause/effect graph: C1 revision R and Environment binding A are recovered
    // into Runtime; C2 reservation pauses; C3 another owner commits binding B at
    // R+1; C4 reservation resumes. Effects: E1 no activity receipt/epoch is
    // committed against R+1; E2 no Run is activated with projection A; E3 the
    // caller gets a retryable admission failure and the durable Session remains
    // Idle on binding B. Exact activity receipt replay is excluded because it is
    // stronger committed recovery truth and is covered by A2 above.
    //
    // | Rule | Projected revision | Root at activity CAS | Effect |
    // |---|---|---|---|
    // | P1 | R | R | ordinary admission (existing A1) |
    // | P2 | R | R+1 | E1-E3, no stale execution |
    let session_id = "projection-revision-race";
    let repository: Arc<dyn ManagedSessionRepository> =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let mut session = persisted(session_id, false, "idle");
    session.environment.set_resident("binding-a");
    create(repository.as_ref(), session).await;
    let entered = Arc::new(tokio::sync::Barrier::new(2));
    let release = Arc::new(tokio::sync::Barrier::new(2));
    let runtime = Arc::new(EventBatchRuntime::default());
    *runtime.reservation_barriers.lock().unwrap() = Some((entered.clone(), release.clone()));
    let app = Arc::new(application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let admission = {
        let app = app.clone();
        tokio::spawn(async move {
            app.admit_session_user_run(SessionUserRunCommand {
                session_id: session_id.into(),
                agent_id: "agent".into(),
                operation_id: "projection-race-operation".into(),
                run_id: RunId("projection-race-run".into()),
                content: vec![ContentBlock::text("do not execute stale projection")],
                accompanying_system: None,
                data_subject_id: None,
                traceparent: None,
            })
            .await
        })
    };
    entered.wait().await;
    let mut replacement = repository.get(&SessionId::from(session_id)).await.unwrap();
    replacement.environment.set_resident("binding-b");
    app.commit_session_snapshot("workspace", replacement, "test-binding-change", Vec::new())
        .await
        .expect("C3 commits the successor root");
    release.wait().await;

    let error = admission
        .await
        .expect("P2 admission task")
        .expect_err("P2 stale projection must retry");
    assert_eq!(
        error.kind,
        awaken_session_contract::RunErrorKind::Unavailable,
        "P2/E3"
    );
    let durable = repository.get(&SessionId::from(session_id)).await.unwrap();
    assert_eq!(durable.environment.binding(), Some("binding-b"), "P2/E3");
    assert_eq!(durable.execution, SessionExecutionState::Idle, "P2/E1");
    assert!(durable.active_activity_epochs.is_empty(), "P2/E1");
    assert!(runtime.activated.lock().unwrap().is_empty(), "P2/E2");
}

#[test]
fn rejected_user_reservation_cannot_orphan_an_accompanying_system() {
    // Coverage rationale: the async case below owns S1 and its E1-E4 oracle.
    // The shared executor changes stack placement only.
    run_composed_async_test(rejected_user_reservation_cannot_orphan_an_accompanying_system_case);
}

async fn rejected_user_reservation_cannot_orphan_an_accompanying_system_case() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Cause/effect graph: C1 the complete initial batch is valid; C2 durable Run
    // projection recovery succeeds; C3 reservation rejects before an activity
    // receipt. Effects: E1 reconciliation reports the failure; E2 projection
    // precedes reservation but no operation receipt is committed; E3 neither
    // System nor User is reserved/committed; E4 no activation occurs.
    // Decision rule S1=C1+C2+C3=>E1+E2+E3+E4.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.fail_reservation.store(true, Ordering::SeqCst);
    let session = planned_session(
        "system-after-admission",
        vec![
            SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("driver")],
            },
            SessionEventInput::SystemMessage {
                content: vec![ContentBlock::text("context")],
            },
        ],
    );
    let run_id = match &session.event_batches[0].events[0].event {
        awaken_session_contract::SessionEventCommand::UserMessage { run_id, .. } => run_id.clone(),
        _ => unreachable!("S1 User command"),
    };
    create(repository.as_ref(), session).await;
    let app = application_with_runtime(
        runtime.clone(),
        repository,
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let report = app.reconcile_event_batches().await;
    assert_eq!(report.failures.len(), 1, "S1/E1");
    let trace = runtime.trace.lock().unwrap().clone();
    let projection = trace
        .iter()
        .position(|entry| entry.starts_with("project:system-after-admission:"))
        .expect("S1/E2 projection");
    let reservation = trace
        .iter()
        .position(|entry| entry == &format!("reserve:{}", run_id.0))
        .expect("S1/E2 rejected reservation");
    assert!(projection < reservation, "S1/E2");
    let operation = awaken_session_contract::session_run_activity_operation_id(
        "system-after-admission",
        &run_id,
    );
    assert!(
        app.recover_activity_for_operation(&SessionId::from("system-after-admission"), &operation)
            .await
            .expect("S1/E2 receipt query")
            .is_none(),
        "S1/E2 no orphan activity"
    );
    assert!(runtime.reserved.lock().unwrap().is_empty(), "S1/E3");
    assert!(runtime.committed.lock().unwrap().is_empty(), "S1/E3");
    assert!(
        runtime
            .trace
            .lock()
            .unwrap()
            .iter()
            .all(|entry| !entry.starts_with("activate:")),
        "S1/E4"
    );
}

#[test]
fn activity_repair_replays_the_same_complete_system_user_reservation() {
    // The shared test executor changes stack placement only; the case below
    // remains the single cause/effect oracle for this recovery boundary.
    run_composed_async_test(activity_repair_replays_the_same_complete_system_user_reservation_case);
}

async fn activity_repair_replays_the_same_complete_system_user_reservation_case() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Crash-repair cause/effect graph: C1 a valid User+System root intent; C2
    // interruption after reservation/activity admission but before activation;
    // C3 cold reconciliation of the same Run. Effects: E1 the Reserved payload
    // already contains both Messages; E2 no partial Message commits at C2; E3 C3
    // reuses the same activity epoch and reservation; E4 activation commits
    // stable System then User and the retained entries advance from Thread truth.
    //
    // | Rule | Reservation | Activity | Activation | Effect |
    // | C1 | absent | absent | interrupted | E1+E2 |
    // | C2 | exact Reserved | existing | succeeds | E3+E4 |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.fail_activation_once.store(true, Ordering::SeqCst);
    let session = planned_session(
        "system-reservation-repair",
        vec![
            SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("driver")],
            },
            SessionEventInput::SystemMessage {
                content: vec![ContentBlock::text("context")],
            },
        ],
    );
    let batch = session.event_batches.first().expect("batch");
    let batch_epoch = batch.wake_activity_epoch.expect("create wake");
    let (run_id, user_operation) = match &batch.events[0].event {
        awaken_session_contract::SessionEventCommand::UserMessage {
            operation_id,
            run_id,
            ..
        } => (run_id.clone(), operation_id.clone()),
        _ => panic!("User command"),
    };
    let system_operation = match &batch.events[1].event {
        awaken_session_contract::SessionEventCommand::SystemMessage { operation_id, .. } => {
            operation_id.clone()
        }
        _ => panic!("System command"),
    };
    create(repository.as_ref(), session).await;
    let warm = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let interrupted = warm.reconcile_event_batches().await;
    assert_eq!(interrupted.failures.len(), 1, "C1");
    let after_interruption = repository
        .get(&SessionId::from("system-reservation-repair"))
        .await
        .expect("C1 durable root");
    let run_epoch = after_interruption.activity_epoch;
    assert_ne!(run_epoch, batch_epoch, "C1/E1 Run activity admitted");
    assert!(
        after_interruption
            .active_activity_epochs
            .contains(&run_epoch),
        "C1/E1"
    );
    {
        let reserved = runtime.reserved.lock().unwrap();
        let command = reserved.get(&run_id.0).expect("C1/E1 reservation");
        let expected_system_id =
            MessageId::session_system("system-reservation-repair", &system_operation);
        assert_eq!(
            command.messages.first().map(|message| &message.id),
            Some(&expected_system_id),
            "C1/E1"
        );
    }
    assert!(runtime.committed.lock().unwrap().is_empty(), "C1/E2");

    let cold = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let repaired = cold.reconcile_event_batches().await;
    assert!(repaired.failures.is_empty(), "C2: {:?}", repaired.failures);
    assert_eq!(runtime.reserved.lock().unwrap().len(), 1, "C2/E3");
    assert_eq!(runtime.activity_epoch(&run_id), run_epoch, "C2/E3");
    assert_eq!(
        repository
            .get(&SessionId::from("system-reservation-repair"))
            .await
            .expect("C2 durable root")
            .activity_epoch,
        run_epoch,
        "C2/E3 no second activity epoch"
    );
    let committed = runtime.committed.lock().unwrap().clone();
    assert_eq!(committed.len(), 2, "C2/E4");
    assert_eq!(
        committed[0].id,
        MessageId::session_system("system-reservation-repair", &system_operation),
        "C2/E4"
    );
    assert_eq!(
        committed[1].id,
        MessageId::session_event_input("system-reservation-repair", &user_operation),
        "C2/E4"
    );

    runtime.end(&run_id);
    finalize_applied_user_dispatch(&cold, &runtime, "system-reservation-repair", &run_id).await;
    let completed = cold.reconcile_event_batches().await;
    assert_eq!(
        completed.settled, 0,
        "C2/E4 post-Dispatch aggregate CAS already settled User+System"
    );
    assert!(
        repository
            .get(&SessionId::from("system-reservation-repair"))
            .await
            .expect("completed root")
            .event_batches
            .first()
            .expect("batch")
            .is_complete(),
        "C2/E4"
    );
}

#[test]
fn event_traceparent_survives_root_persistence_and_crash_recovery() {
    // Coverage rationale: the async case below owns the full T1/T2 table. This
    // wrapper only selects the shared composed-test executor and adds no oracle.
    run_composed_async_test(event_traceparent_survives_root_persistence_and_crash_recovery_case);
}

async fn event_traceparent_survives_root_persistence_and_crash_recovery_case() {
    // Cause/effect graph: C1 admission trace context is present/absent; C2 the
    // root batch commits; C3 activation fails after reservation and a cold
    // supervisor retries. Effects: E1 root provenance and the first reservation
    // carry the exact C1 value; E2 recovery rebuilds the same command from the
    // root and reuses one reservation; E3 absence remains None rather than being
    // replaced by the recovery task's ambient context.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // | T1 | present | yes | crash+recover | E1+E2 |
    // | T2 | absent | yes | crash+recover | E1+E2+E3 |
    // Constraint/invariant: the Session root is the sole trace provenance for
    // Event Run recovery; trace context never selects a new Run or reservation.
    // Initial Events intentionally stay None because this change is bounded to
    // ordinary POST `/events` admission, which owns the admitting request span.
    const TRACEPARENT: &str = "00-11111111111111111111111111111111-2222222222222222-01";
    for (case, expected_traceparent) in
        [("present", Some(TRACEPARENT.to_string())), ("absent", None)]
    {
        let session_id = format!("event-trace-{case}");
        let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("trace recovery repository"),
        );
        create(repository.as_ref(), persisted(&session_id, false, "idle")).await;
        let runtime = Arc::new(EventBatchRuntime::default());
        runtime.fail_activation_once.store(true, Ordering::SeqCst);
        let warm = application_with_runtime(
            runtime.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let admitted = warm
            .append_session_event_batch(
                &session_id,
                vec![SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("trace me")],
                }],
                None,
                expected_traceparent.clone(),
            )
            .await
            .expect("T1/T2 root admission");
        let run_id = match &admitted.events[0].event {
            awaken_session_contract::SessionEventCommand::UserMessage { run_id, .. } => {
                run_id.clone()
            }
            _ => panic!("T1/T2 User command"),
        };
        assert_eq!(admitted.traceparent, expected_traceparent, "T1/T2/E1");

        let interrupted = warm.reconcile_event_batches().await;
        assert_eq!(interrupted.failures.len(), 1, "T1/T2/C3");
        assert_eq!(
            runtime
                .reserved
                .lock()
                .unwrap()
                .get(&run_id.0)
                .expect("T1/T2 first reservation")
                .traceparent,
            expected_traceparent,
            "T1/T2/E1"
        );

        let cold = application_with_runtime(
            runtime.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let recovered = cold.reconcile_event_batches().await;
        assert!(
            recovered.failures.is_empty(),
            "T1/T2/E2: {:?}",
            recovered.failures
        );
        let reservations = runtime.reserved.lock().unwrap();
        assert_eq!(reservations.len(), 1, "T1/T2/E2 one reservation");
        assert_eq!(
            reservations
                .get(&run_id.0)
                .expect("T1/T2 recovered reservation")
                .traceparent,
            expected_traceparent,
            "T1/T2/E2+E3"
        );
    }
}

#[tokio::test]
async fn ordinary_active_active_append_is_atomic_revision_ordered_and_activity_free() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Ordinary append cause/effect table. C1 two writers read the same Session
    // revision; C2 each batch has one/multiple immutable entries; C3 each
    // request has distinct data-subject attribution. Effects: E1 root CAS
    // linearizes both batches at distinct consecutive revisions; E2 every
    // winning snapshot exposes the whole batch or none; E3 User attribution is
    // frozen per command; E4 acceptance opens no activity. Rules A1 one winner
    // at revision N+1, loser reloads and wins N+2; A2 every persisted batch has
    // its complete entry count; A3 queued acceptance keeps Session Idle.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    create(
        repository.as_ref(),
        persisted("ordinary-atomic", false, "idle"),
    )
    .await;
    let runtime = Arc::new(EventBatchRuntime::default());
    let app = Arc::new(application_with_runtime(
        runtime,
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let first_app = Arc::clone(&app);
    let second_app = Arc::clone(&app);
    let first = tokio::spawn(async move {
        first_app
            .append_session_event_batch(
                "ordinary-atomic",
                vec![
                    SessionEventInput::UserMessage {
                        content: vec![ContentBlock::text("first")],
                    },
                    SessionEventInput::SystemMessage {
                        content: vec![ContentBlock::text("context")],
                    },
                ],
                Some("subject-a".into()),
                None,
            )
            .await
    });
    let second = tokio::spawn(async move {
        second_app
            .append_session_event_batch(
                "ordinary-atomic",
                vec![SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("second")],
                }],
                Some("subject-b".into()),
                None,
            )
            .await
    });
    first.await.unwrap().expect("A1 first append");
    second.await.unwrap().expect("A1 second append");

    let durable = repository
        .get(&SessionId::from("ordinary-atomic"))
        .await
        .unwrap();
    assert_eq!(durable.revision.0, 3, "A1/E1");
    assert_eq!(durable.event_batches.len(), 2, "A1/E1");
    assert_eq!(
        durable
            .event_batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            awaken_session_contract::session_event_batch_id(
                "ordinary-atomic",
                awaken_session_contract::SessionRevision(2),
            )
            .unwrap(),
            awaken_session_contract::session_event_batch_id(
                "ordinary-atomic",
                awaken_session_contract::SessionRevision(3),
            )
            .unwrap(),
        ],
        "A1/E1"
    );
    assert_eq!(
        durable
            .event_batches
            .iter()
            .map(|batch| batch.events.len())
            .sum::<usize>(),
        3,
        "A2/E2 no half-batch"
    );
    let mut subjects = durable
        .event_batches
        .iter()
        .flat_map(|batch| &batch.events)
        .filter_map(|entry| match &entry.event {
            awaken_session_contract::SessionEventCommand::UserMessage {
                data_subject_id, ..
            } => data_subject_id.clone(),
            _ => None,
        })
        .collect::<Vec<_>>();
    subjects.sort();
    assert_eq!(subjects, ["subject-a", "subject-b"], "A3/E3");
    assert!(
        durable
            .event_batches
            .iter()
            .all(|batch| batch.wake_activity_epoch.is_none()
                && batch.events.iter().all(|entry| !entry.processed)),
        "A2+A3/E2+E4"
    );
    assert!(durable.active_activity_epochs.is_empty(), "A3/E4");
    assert_eq!(
        durable.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "A3/E4"
    );
}

#[test]
fn outcome_crosses_awaiting_user_while_next_user_stays_queued_after_restart() {
    // Coverage rationale: the async case below owns Q1-Q3. The shared executor
    // changes stack placement only and does not add another recovery oracle.
    run_composed_async_test(
        outcome_crosses_awaiting_user_while_next_user_stays_queued_after_restart_case,
    );
}

#[test]
fn queued_user_before_awaiting_boundary_must_not_hide_required_tool_action() {
    run_composed_async_test(queued_user_before_awaiting_boundary_case);
}

async fn queued_user_before_awaiting_boundary_case() {
    for legacy in [false, true] {
        queued_input_before_awaiting_boundary(false, legacy).await;
        queued_input_before_awaiting_boundary(true, legacy).await;
    }
}

async fn queued_input_before_awaiting_boundary(with_system: bool, legacy: bool) {
    // Live Classroom regression: C1 first User is executing; C2 a later User
    // is accepted BEFORE first Awaiting settlement; C3 first Dispatch settles;
    // C4 a cold supervisor reconciles the same root. Effects: E1 first input
    // processed with no active epoch; E2 later User unprocessed/unreserved;
    // E3 Session Idle exposes the exact required action instead of withholding
    // it until that blocked later User executes. Rule AQ1 C1+C2+C3+C4=>E1-E3.
    // C5 later User carries/does not carry its paired System input; both remain
    // unprocessed together. A System after a ToolReply is not this partition.
    // The existing outcome-crosses-awaiting test covers the opposite order
    // (C2 AFTER C3); both orders must retain the same actionability and queue.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("AQ1 repository"),
    );
    let id = "queued-before-awaiting";
    create(repository.as_ref(), persisted(id, false, "idle")).await;
    let runtime = Arc::new(EventBatchRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    for (key, content) in [("first", "first"), ("later", "queued")] {
        let mut inputs = vec![SessionEventInput::UserMessage {
            content: vec![ContentBlock::text(content)],
        }];
        if key == "later" && with_system {
            inputs.push(SessionEventInput::SystemMessage {
                content: vec![ContentBlock::text("paired later context")],
            });
        }
        app.append_session_event_batch(id, inputs, Some(key.into()), None)
            .await
            .expect("AQ1 append");
        assert!(app.reconcile_event_batches().await.failures.is_empty());
    }
    let run = runtime
        .latest_run
        .lock()
        .unwrap()
        .clone()
        .expect("AQ1 first Run");
    assert_eq!(
        runtime.reserved.lock().unwrap().len(),
        1,
        "AQ1 later stays queued"
    );
    runtime.await_run(&run);
    if legacy {
        // AQ2 reproduces the old policy (no Awaiting-specific closure) through
        // its conservative Completed branch, then reopens the SQLite-backed
        // application against the real fixture's Awaiting Run. The exact
        // Dispatch receipt/interval remain durable; no SQL repair is used.
        let epoch = runtime.activity_epoch(&run);
        app.settle_user_event_dispatch_observed(
            &SessionId::from(id),
            epoch,
            awaken_session_contract::SessionRuntimeIntervalObservation {
                activity_epoch: epoch,
                thread_id: ThreadId(id.into()),
                run_id: run.clone(),
                lifecycle_cursor: awaken_agent_contract::RunLifecycleCursor(epoch),
                source_commit_cursor: 1,
            },
            &run,
            awaken_session_contract::SessionAgentRunBoundary::Completed,
        )
        .await
        .expect("AQ2 old conservative settlement");
        assert_eq!(
            repository
                .get(&SessionId::from(id))
                .await
                .unwrap()
                .execution,
            awaken_session_contract::SessionExecutionState::Running,
            "AQ2 stranded legacy shape"
        );
    } else {
        finalize_applied_user_dispatch(&app, &runtime, id, &run).await;
    }
    let cold = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert!(cold.reconcile_event_batches().await.failures.is_empty());
    let root = repository
        .get(&SessionId::from(id))
        .await
        .expect("AQ1 durable root");
    assert!(root.event_batches[0].events[0].processed, "AQ1/E1");
    assert!(root.active_activity_epochs.is_empty(), "AQ1/E1");
    assert!(!root.event_batches[1].events[0].processed, "AQ1/E2");
    assert!(
        root.event_batches[1]
            .events
            .iter()
            .all(|entry| !entry.processed),
        "AQ1/E2 paired System stays queued"
    );
    assert_eq!(runtime.reserved.lock().unwrap().len(), 1, "AQ1/E2");
    assert_eq!(
        root.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "AQ1/E3 queued successor cannot hide the first Run's required action"
    );
    // AQ3 exact repeated recovery is a complete root stutter; AQ4 a later
    // active epoch must survive an old callback. Compare the whole serialized
    // aggregate, not merely status, so receipts and timestamps cannot drift.
    let stable = serde_json::to_value(&root).unwrap();
    assert!(cold.reconcile_event_batches().await.failures.is_empty());
    assert_eq!(
        serde_json::to_value(repository.get(&SessionId::from(id)).await.unwrap()).unwrap(),
        stable,
        "AQ3"
    );
    let (_, newer_epoch) = cold
        .begin_activity_for_operation(&SessionId::from(id), "newer-execution")
        .await
        .unwrap();
    assert!(newer_epoch > runtime.activity_epoch(&run));
    let newer = serde_json::to_value(repository.get(&SessionId::from(id)).await.unwrap()).unwrap();
    finalize_applied_user_dispatch(&cold, &runtime, id, &run).await;
    assert_eq!(
        serde_json::to_value(repository.get(&SessionId::from(id)).await.unwrap()).unwrap(),
        newer,
        "AQ4 old callback cannot close a newer activity"
    );
}

async fn outcome_crosses_awaiting_user_while_next_user_stays_queued_after_restart_case() {
    // Eligibility/recovery decision table. C1 first User is Running/Awaiting;
    // C2 a later batch contains User then DefineOutcome; C3 one/two restarted
    // supervisors race. Effects: E1 Running User remains unprocessed; E2
    // Awaiting proves that User processed and its activity settles; E3 Outcome
    // exact prepare crosses the queued User and is processed on receipt; E4 the
    // later User remains queued with no reservation/activity. Rules Q1 Running
    // =>E1; Q2 Awaiting+later Outcome=>E2+E3+E4; Q3 active-active replay=>one
    // Outcome aggregate and the same retained flags.
    // Causes: C1-C3 select Running/Awaiting, later User+Outcome, and restarted
    // supervisor races. Constraint/Invariant: Outcome may cross only proven
    // Awaiting User truth; the later User keeps queue order. Decision rule:
    // execute Q1-Q3 and require one Outcome plus retained later User.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    create(
        repository.as_ref(),
        persisted("ordinary-eligibility", false, "idle"),
    )
    .await;
    let runtime = Arc::new(EventBatchRuntime::default());
    let warm = Arc::new(application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    warm.append_session_event_batch(
        "ordinary-eligibility",
        vec![SessionEventInput::UserMessage {
            content: vec![ContentBlock::text("first")],
        }],
        Some("subject-first".into()),
        None,
    )
    .await
    .unwrap();
    let running = warm.reconcile_event_batches().await;
    assert!(running.failures.is_empty(), "Q1");
    let first_run = runtime.latest_run.lock().unwrap().clone().expect("Q1 Run");
    assert!(
        !repository
            .get(&SessionId::from("ordinary-eligibility"))
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "Q1/E1"
    );

    runtime.await_run(&first_run);
    finalize_applied_user_dispatch(&warm, &runtime, "ordinary-eligibility", &first_run).await;
    warm.append_session_event_batch(
        "ordinary-eligibility",
        vec![
            SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("queued")],
            },
            SessionEventInput::DefineOutcome {
                description: "ship".into(),
                rubric: SessionOutcomeRubric::Text {
                    content: "correct".into(),
                },
                max_iterations: Some(2),
            },
        ],
        Some("subject-queued".into()),
        None,
    )
    .await
    .unwrap();

    let cold_one = Arc::new(application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let cold_two = Arc::new(application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let left = tokio::spawn(async move { cold_one.reconcile_event_batches().await });
    let right = tokio::spawn(async move { cold_two.reconcile_event_batches().await });
    let left = left.await.unwrap();
    let right = right.await.unwrap();
    assert!(left.failures.is_empty() && right.failures.is_empty(), "Q3");

    let durable = repository
        .get(&SessionId::from("ordinary-eligibility"))
        .await
        .unwrap();
    assert!(durable.event_batches[0].events[0].processed, "Q2/E2");
    assert!(!durable.event_batches[1].events[0].processed, "Q2/E4");
    assert!(durable.event_batches[1].events[1].processed, "Q2/E3");
    assert_eq!(runtime.outcomes.lock().unwrap().len(), 1, "Q3/E3");
    assert_eq!(runtime.reserved.lock().unwrap().len(), 1, "Q2/E4");
    assert!(durable.active_activity_epochs.is_empty(), "Q2/E4");
    assert_eq!(
        durable.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "Q2/E2+E4"
    );
}

#[tokio::test]
async fn outcome_receipt_is_processed_on_stable_prepare_and_cold_replay_skips_it() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Outcome receipt cause/effect table. C1 the stable Outcome aggregate is
    // absent/present; C2 process is warm/restarted; C3 create wake is active.
    // Effects: E1 exact prepare once; E2 mark the retained entry processed on
    // prepare receipt with its exact durable cursor; E3 settle create wake immediately; E4 never hold receipt
    // completion on Outcome evaluation/continue. Rules O1 absent+warm=>E1-E4;
    // O2 present+processed+cold=>skip every effect and retain provenance.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    let session = planned_session(
        "initial-outcome",
        vec![SessionEventInput::DefineOutcome {
            description: "ship".into(),
            rubric: SessionOutcomeRubric::Text {
                content: "FINAL".into(),
            },
            max_iterations: Some(2),
        }],
    );
    let (batch_epoch, expected_outcome_id) = {
        let batch = session.event_batches.first().expect("batch");
        let awaken_session_contract::SessionEventCommand::DefineOutcome { outcome_id, .. } =
            &batch.events[0].event
        else {
            panic!("Outcome Event")
        };
        (
            batch.wake_activity_epoch.expect("create wake"),
            outcome_id.clone(),
        )
    };
    create(repository.as_ref(), session).await;
    let warm = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let completed = warm.reconcile_event_batches().await;
    assert!(completed.failures.is_empty(), "O1/E1");
    assert_eq!(completed.settled, 1, "O1/E2");
    assert_eq!(runtime.outcomes.lock().unwrap().len(), 1, "O1/E1");
    let durable = repository
        .get(&SessionId::from("initial-outcome"))
        .await
        .unwrap();
    assert!(durable.event_batches[0].events[0].processed, "O1/E2");
    assert_eq!(
        durable.event_batches[0].events[0]
            .projection_anchor
            .map(|anchor| anchor.source_commit_cursor),
        Some(1),
        "O1/E2 exact Outcome prepare cursor"
    );
    assert!(
        !durable.active_activity_epochs.contains(&batch_epoch),
        "O1/E3"
    );

    let cold = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let replay = cold.reconcile_event_batches().await;
    assert!(replay.failures.is_empty(), "O2");
    assert_eq!(replay.settled, 0, "O2");
    assert_eq!(runtime.outcomes.lock().unwrap().len(), 1, "O2/E1");
    assert!(
        runtime
            .outcome_prepare_calls
            .lock()
            .unwrap()
            .iter()
            .all(|id| id == &expected_outcome_id),
        "O1/E1 stable id"
    );
}

#[tokio::test]
async fn outcome_busy_is_retryable_and_cannot_mark_the_root_entry_processed() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the stable DefineOutcome command is unprocessed;
    // C2 another Outcome owns the Thread aggregate; C3 that owner later clears.
    // Effects: E1 Busy is retryable rather than a reconciliation failure; E2
    // root provenance remains unprocessed during C2; E3 C3 prepares exactly the
    // retained id and only then commits processed=true.
    //
    // | Rule | Root command | Aggregate | Effect |
    // | B1 | unprocessed | busy | E1 + E2 |
    // | B2 | same retained command | available | E3 |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.outcome_busy.store(true, Ordering::SeqCst);
    create(
        repository.as_ref(),
        planned_session(
            "outcome-busy",
            vec![SessionEventInput::DefineOutcome {
                description: "ship".into(),
                rubric: SessionOutcomeRubric::Text {
                    content: "correct".into(),
                },
                max_iterations: None,
            }],
        ),
    )
    .await;
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let busy = app.reconcile_event_batches().await;
    assert!(busy.failures.is_empty(), "B1/E1");
    assert_eq!(busy.pending, 1, "B1/E1");
    assert!(
        !repository
            .get(&SessionId::from("outcome-busy"))
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "B1/E2"
    );

    runtime.outcome_busy.store(false, Ordering::SeqCst);
    let prepared = app.reconcile_event_batches().await;
    assert!(prepared.failures.is_empty(), "B2/E3");
    assert!(
        repository
            .get(&SessionId::from("outcome-busy"))
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "B2/E3"
    );
}

#[test]
fn initial_outcome_reconciliation_does_not_wait_for_its_own_first_run() {
    // Cause/effect graph: C1 retained DefineOutcome is processed; C2 Session is
    // Idle or awaiting initial realization; C3 an activity still owns a Run.
    // R1 C1+C2+!C3 -> select the existing Outcome continuation, whose normal Run
    // admission owns realization. R2 !C1 -> no drive before durable preparation.
    // R3 C3 -> no duplicate Worker Run. R4 Running/Rescheduling/terminal -> no
    // competing driver. The live Managed initial_events probe exposed R1 at
    // Preparing: no first Run could be created while the selector required Idle.
    use awaken_session_contract::SessionExecutionState as State;
    for state in [
        State::Preparing,
        State::Activating,
        State::Idle,
        State::Running,
        State::Rescheduling,
        State::ActivationFailed,
        State::Terminated,
    ] {
        for processed in [false, true] {
            for active in [false, true] {
                let mut session = planned_session(
                    "initial-outcome-selection",
                    vec![SessionEventInput::DefineOutcome {
                        description: "ship".into(),
                        rubric: SessionOutcomeRubric::Text {
                            content: "verified artifact".into(),
                        },
                        max_iterations: Some(3),
                    }],
                );
                session.execution = state;
                session.active_activity_epochs.clear();
                if active {
                    session.active_activity_epochs.insert(1);
                }
                session.event_batches[0].events[0].processed = processed;
                let expected = processed
                    && !active
                    && matches!(state, State::Preparing | State::Activating | State::Idle);
                assert_eq!(
                    session.needs_outcome_reconciliation(),
                    expected,
                    "state={state:?}, processed={processed}, active={active}"
                );
            }
        }
    }
}

#[tokio::test]
async fn cold_supervisor_continues_only_the_thread_owned_outcome_truth() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Crash/recovery cause/effect graph: C1 root prepare is durably processed;
    // C2 the process stops before Outcome continuation; C3 the Thread aggregate
    // is active/terminal/inactive. Effects: E1 a cold supervisor selects the
    // Thread from retained root provenance; E2 Awaiting remains pending; E3 a
    // completed aggregate settles once; E4 inactive truth performs no effect.
    // The conservative root selector stores no active/terminal shadow.
    //
    // | Rule | Root provenance | Thread truth | Effect |
    // | C1 | retained+processed | active | E1 + E2 |
    // | C2 | retained+processed | terminal transition | E3 |
    // | C3 | retained+processed | inactive | E4 |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    create(
        repository.as_ref(),
        planned_session(
            "outcome-cold-continuation",
            vec![SessionEventInput::DefineOutcome {
                description: "ship".into(),
                rubric: SessionOutcomeRubric::File {
                    file_id: "file_rubric".into(),
                },
                max_iterations: None,
            }],
        ),
    )
    .await;
    let warm = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let prepared = warm.reconcile_event_batches().await;
    assert!(prepared.failures.is_empty(), "C1 prepare");
    let durable = repository
        .get(&SessionId::from("outcome-cold-continuation"))
        .await
        .unwrap();
    assert!(
        matches!(
            &durable.event_batches[0].events[0].event,
            awaken_session_contract::SessionEventCommand::DefineOutcome {
                rubric: SessionOutcomeRubric::File { file_id },
                max_iterations: None,
                ..
            } if file_id == "file_rubric"
        ),
        "C1/E1 exact public provenance"
    );
    assert_eq!(
        runtime.outcomes.lock().unwrap().values().next().cloned(),
        Some(("ship".into(), "file_rubric".into(), 3)),
        "C1/E1 execution lowering applies the existing default only at effect time"
    );
    let mut in_flight = durable.clone();
    in_flight.execution = awaken_session_contract::SessionExecutionState::Running;
    assert!(
        !in_flight.needs_outcome_reconciliation(),
        "C1: a retained Outcome cannot replay its stable Worker Run while Session activity is in flight"
    );
    drop(warm);

    let cold = application_with_runtime(
        runtime.clone(),
        repository,
        Arc::new(RecordingEnvironmentSource::default()),
    );
    runtime.dispatch_projection_installs.lock().unwrap().clear();
    let awaiting = cold.reconcile_outcome_continuations().await;
    assert_eq!(awaiting.pending, 1, "C1/E1+E2");
    assert_eq!(runtime.outcome_continue_calls.load(Ordering::SeqCst), 1);
    // C4/E5 a replacement process must restore this Session's exact frozen
    // dispatch projection before continuing its Outcome. Without it Session
    // admission would fall back to the flat-protocol `assistant` identity and
    // the Outcome would pin and grade an Agent that was never published.
    assert_eq!(
        runtime
            .dispatch_projection_installs
            .lock()
            .unwrap()
            .as_slice(),
        ["outcome-cold-continuation"],
        "C4/E5 exactly one restore of the continued Session precedes its Outcome"
    );

    runtime.outcome_completed.store(true, Ordering::SeqCst);
    let completed = cold.reconcile_outcome_continuations().await;
    assert_eq!(completed.settled, 1, "C2/E3");
    let inactive = cold.reconcile_outcome_continuations().await;
    assert_eq!(
        inactive,
        super::super::outcome_reconciliation::OutcomeReconciliation::default(),
        "C3/E4"
    );
    assert_eq!(runtime.outcome_continue_calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn resumed_user_run_dispatch_settles_each_activity_without_republishing_input() {
    // Cause/effect graph: first User+System batch -> Applied Dispatch at Awaiting
    // -> new approval activity on SAME Run -> Applied Dispatch at next Awaiting
    // -> another activity -> terminal Applied Dispatch. R1 every boundary must
    // remove its own activity and close Running; R2 exact/stale callback replay
    // cannot change the committed root; R3 first input timestamps and anchors
    // stay immutable; R4 cold Session application reads the same persisted fence.
    // This fixture supplies Dispatch-Applied at the existing application port;
    // real Worker/HTTP and Native/ACP approval journeys own the transport proof.
    // R5 format=current/legacy: the supported old wire omits dispatch_settlement
    // after processing. Persist that shape through the existing SQLite owner,
    // then require cold continuation to establish its first NEW Dispatch receipt
    // without changing published input. This is a stored-format regression;
    // mixed executable versions remain a separate real deployment acceptance.
    for legacy in [false, true] {
        let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let runtime = Arc::new(EventBatchRuntime::default());
        let id = "resumed-user-dispatch";
        let session = planned_session(
            id,
            vec![
                SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("work")],
                },
                SessionEventInput::SystemMessage {
                    content: vec![ContentBlock::text("context")],
                },
            ],
        );
        let run = match &session.event_batches[0].events[0].event {
            SessionEventCommand::UserMessage { run_id, .. } => run_id.clone(),
            _ => unreachable!(),
        };
        create(repository.as_ref(), session).await;
        let app = application_with_runtime(
            runtime.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        assert!(app.reconcile_event_batches().await.failures.is_empty());
        runtime.await_run(&run);
        finalize_applied_user_dispatch(&app, &runtime, id, &run).await;
        let first = repository.get(&SessionId::from(id)).await.unwrap();
        assert_eq!(
            first.execution,
            awaken_session_contract::SessionExecutionState::Idle
        );
        let published = |root: &PersistedSession| {
            root.event_batches[0]
                .events
                .iter()
                .map(|entry| {
                    (
                        entry.processed,
                        entry.projection_anchor,
                        entry.processed_at_unix_ms,
                    )
                })
                .collect::<Vec<_>>()
        };
        let original = published(&first);
        if legacy {
            let mut wire = serde_json::to_value(&first).unwrap();
            for entry in wire["event_batches"][0]["events"].as_array_mut().unwrap() {
                entry.as_object_mut().unwrap().remove("dispatch_settlement");
            }
            let old: PersistedSession = serde_json::from_value(wire).unwrap();
            let expected_revision = old.revision;
            let payload = awaken_session_contract::SessionMutationPayload::Replace(old);
            let payload_hash = payload.stable_hash();
            assert!(matches!(
                repository
                    .commit_mutation(
                        "workspace",
                        awaken_session_contract::SessionMutation {
                            expected_revision,
                            idempotency: awaken_session_contract::IdempotencyRecord {
                                key: "legacy:processed-user-without-dispatch-receipt".into(),
                                payload_hash,
                            },
                            payload,
                            lifecycle_facts: Vec::new(),
                        }
                    )
                    .await
                    .unwrap(),
                awaken_session_contract::SessionMutationResult::Applied { .. }
            ));
            let decoded = repository.get(&SessionId::from(id)).await.unwrap();
            assert!(
                decoded.event_batches[0].events[0]
                    .dispatch_settlement
                    .is_none()
            );
            assert_eq!(
                published(&decoded),
                original,
                "R5 old publication is valid stored input"
            );
        }
        let mut previous = None;
        for cursor in [2, 3] {
            let boundary = if cursor == 2 {
                awaken_session_contract::SessionAgentRunBoundary::Awaiting
            } else {
                awaken_session_contract::SessionAgentRunBoundary::Completed
            };
            // Recreate the stateless application to exercise persisted receipts.
            let cold = application_with_runtime(
                runtime.clone(),
                repository.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
            );
            let (_, epoch) = cold
                .begin_activity_for_operation(&SessionId::from(id), &format!("approval-{cursor}"))
                .await
                .unwrap();
            let observation = awaken_session_contract::SessionRuntimeIntervalObservation {
                activity_epoch: epoch,
                thread_id: ThreadId(id.into()),
                run_id: run.clone(),
                lifecycle_cursor: awaken_agent_contract::RunLifecycleCursor(cursor),
                source_commit_cursor: cursor,
            };
            let root = cold
                .settle_user_event_dispatch_observed(
                    &SessionId::from(id),
                    epoch,
                    observation.clone(),
                    &run,
                    boundary,
                )
                .await
                .unwrap();
            assert_eq!(
                root.execution,
                awaken_session_contract::SessionExecutionState::Idle,
                "R1"
            );
            assert!(
                root.active_activity_epochs.is_empty(),
                "R1 no stranded approval activity"
            );
            assert_eq!(published(&root), original, "R3");
            let bytes = serde_json::to_value(&root).unwrap();
            cold.settle_user_event_dispatch_observed(
                &SessionId::from(id),
                epoch,
                observation.clone(),
                &run,
                boundary,
            )
            .await
            .unwrap();
            if let Some((old_epoch, old_observation)) = previous {
                cold.settle_user_event_dispatch_observed(
                    &SessionId::from(id),
                    old_epoch,
                    old_observation,
                    &run,
                    boundary,
                )
                .await
                .unwrap();
            }
            assert_eq!(
                serde_json::to_value(repository.get(&SessionId::from(id)).await.unwrap()).unwrap(),
                bytes,
                "R2/R4"
            );
            previous = Some((epoch, observation));
        }
        assert_eq!(
            runtime.reserved.lock().unwrap().len(),
            1,
            "no User input replay"
        );
    }
}

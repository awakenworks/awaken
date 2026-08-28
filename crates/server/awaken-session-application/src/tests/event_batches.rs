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
    reserved: Mutex<BTreeMap<String, AdmitSessionRun>>,
    projected_resources: Mutex<Option<(u64, awaken_session_contract::ResolvedSessionResources)>>,
    reserved_resource_generations:
        Mutex<BTreeMap<String, (u64, awaken_session_contract::ResolvedSessionResources)>>,
    reservation_outcomes: Mutex<VecDeque<SessionRunReservation>>,
    fail_reservation: AtomicBool,
    fail_activation_once: AtomicBool,
    activated: Mutex<BTreeMap<String, u64>>,
    states: Mutex<HashMap<String, RunState>>,
    latest_run: Mutex<Option<RunId>>,
    committed: Mutex<Vec<Message>>,
    trace: Mutex<Vec<String>>,
    outcomes: Mutex<BTreeMap<String, (String, String, u32)>>,
    outcome_commit_cursors: Mutex<BTreeMap<String, u64>>,
    next_outcome_commit_cursor: AtomicU64,
    outcome_prepare_calls: Mutex<Vec<String>>,
    outcome_busy: AtomicBool,
    outcome_active: AtomicBool,
    outcome_continue_calls: AtomicUsize,
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
                MessageId::session_system(&command.session_id, &system.operation_id),
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
        let message_commit_cursors = (0..messages.len())
            .map(|index| index as u64 + 1)
            .collect::<Vec<_>>();
        let store_cursor = message_commit_cursors.last().copied().unwrap_or(1);
        (messages, message_commit_cursors, store_cursor)
    }
}

#[async_trait::async_trait]
impl SessionRuntime for EventBatchRuntime {
    async fn prepare_session(
        &self,
        thread: &str,
        init: awaken_session_contract::SessionInit,
    ) -> Result<(), RunError> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("project:{thread}:{}", init.resource_revision));
        *self.projected_resources.lock().unwrap() = Some((init.resource_revision, init.resources));
        Ok(())
    }

    async fn reserve_session_run(
        &self,
        command: AdmitSessionRun,
    ) -> Result<SessionRunReservation, RunError> {
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
        _session_id: &str,
        run_id: &RunId,
    ) -> Result<Option<RunState>, RunError> {
        Ok(self.states.lock().unwrap().get(&run_id.0).cloned())
    }

    async fn committed_messages(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(self.committed.lock().unwrap().clone())
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        let staged_run_id = self
            .staged_tool_replies
            .lock()
            .unwrap()
            .values()
            .find(|command| command.target.thread_id(session_id).0 == thread_id)
            .map(|command| command.expected_run_id.clone());
        if let Some(run_id) = staged_run_id {
            let thread_id = ThreadId(thread_id.to_string());
            let (messages, message_commit_cursors, store_cursor) =
                self.committed_message_projection();
            return Ok(Some(
                awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
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
        session_id: &str,
    ) -> Result<Vec<CoordinatedThreadLink>, RunError> {
        Ok(vec![CoordinatedThreadLink {
            session_id: session_id.to_string(),
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
                prior_session_activity_epoch: None,
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
            prior_session_activity_epoch: Some(prior_session_activity_epoch),
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
        session_id: &str,
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

fn tool_reply_input(tool_use_id: &str) -> SessionEventInput {
    SessionEventInput::ToolReply(SessionEventToolReply {
        tool_request_event_id: format!("evt_{tool_use_id}"),
        target: SessionThreadTarget::Primary,
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

fn running_session_with_activity(session_id: &str, epoch: u64) -> PersistedSession {
    let mut session = persisted(session_id, false, "running");
    session.activity_epoch = epoch;
    session.active_activity_epochs.insert(epoch);
    session
}

#[tokio::test]
async fn cancelled_user_run_anchors_its_receipt_without_inventing_a_message() {
    // Cause/effect graph: C1 an accepted User command owns exact Run R; C2
    // interruption ends R as Cancelled before its input Message commits; C3 the
    // same shape ends for another cause. Effects: E1 C1+C2 anchors the receipt at
    // R's terminal store cursor and completes the FIFO; E2 no Message is invented;
    // E3 C1+C3 remains pending/fail-closed because only cancellation explains the
    // absent Message. Constraint: the retained Session command and Runtime Run
    // snapshot remain the only authorities; no abandonment ledger is introduced.
    // Decision table: U1=C1+C2 -> E1+E2; U2=C1+C3 -> E2+E3.
    for (session_id, end, should_complete) in [
        ("cancelled-user-receipt", EndCause::Cancelled, true),
        ("noncancelled-user-receipt", EndCause::NaturalEnd, false),
    ] {
        let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("U1-U2 repository"),
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
            .insert(run_id.0.clone(), RunState::Ended(end));
        *runtime.latest_run.lock().unwrap() = Some(run_id);
        let app = application_with_runtime(
            runtime.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );

        let result = app.drive_session_event_batches(session_id, None).await;
        assert_eq!(result.is_ok(), should_complete, "U1-U2 reconciliation");
        let persisted = repository.get(session_id).await.expect("U1-U2 root");
        let entry = &persisted.event_batches[0].events[0];
        assert_eq!(entry.processed, should_complete, "U1/E1 or U2/E3");
        assert_eq!(
            entry
                .projection_anchor
                .map(|anchor| anchor.source_commit_cursor),
            should_complete.then_some(1),
            "U1/E1 exact Run cursor; U2/E3 no guessed anchor",
        );
        assert!(runtime.committed.lock().unwrap().is_empty(), "U1-U2/E2");
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
            "event-idempotency",
            input.clone(),
            None,
            None,
            Some(("key-1".into(), "fingerprint-1".into())),
        )
        .await
        .expect("I1/E1");
    let replay = app
        .append_session_event_batch_idempotent(
            "event-idempotency",
            input.clone(),
            None,
            None,
            Some(("key-1".into(), "fingerprint-1".into())),
        )
        .await
        .expect("I2/E2 warm replay");
    assert_eq!(replay, first, "I2/E2");
    assert_eq!(
        repository
            .get("event-idempotency")
            .await
            .unwrap()
            .event_batches
            .len(),
        1,
        "I2/E2"
    );

    let restarted = application_with_runtime(
        Arc::new(EventBatchRuntime::default()),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert_eq!(
        restarted
            .session_event_batch_idempotency("event-idempotency", "key-1", "fingerprint-1",)
            .await
            .expect("I2 restart read"),
        SessionEventBatchIdempotency::Exact(first.clone()),
        "I2/E2 restart replay"
    );
    let conflict = restarted
        .append_session_event_batch_idempotent(
            "event-idempotency",
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
            "event-idempotency",
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

    let (equal_a, equal_b) = tokio::join!(
        replica_a.append_session_event_batch_idempotent(
            "event-race-equal",
            input.clone(),
            None,
            None,
            Some(("shared-key".into(), "same-fingerprint".into())),
        ),
        replica_b.append_session_event_batch_idempotent(
            "event-race-equal",
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
            .get("event-race-equal")
            .await
            .unwrap()
            .event_batches
            .len(),
        1,
        "C1/E3 one root batch",
    );

    let (different_a, different_b) = tokio::join!(
        replica_a.append_session_event_batch_idempotent(
            "event-race-conflict",
            input.clone(),
            None,
            None,
            Some(("shared-key".into(), "fingerprint-a".into())),
        ),
        replica_b.append_session_event_batch_idempotent(
            "event-race-conflict",
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
            .get("event-race-conflict")
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
    // Cause/effect graph: C1 an old writer left a terminal Session with one
    // accepted unprocessed command; C2 terminal cleanup is Fenced, Requested
    // with a cursor, or a legacy Requested row with the cursor field absent;
    // C3 the fence later freezes an exact Runtime cursor. Effects: E1 the
    // canonical batch supervisor executes no Runtime effect; E2 Fenced remains
    // incomplete; E3 Requested/Completed resolves under root CAS; E4 an exact
    // cursor becomes the projection anchor while legacy Requested+None remains
    // the explicit processed/no-anchor prefix. Rules T1=C1+Fenced=>E1+E2;
    // T2=C1+Requested(Some)=>E1+E3+E4; T3=C1+legacy Requested(None)=>E1+E3+E4;
    // T4=T1+C3=>E1+E3+E4. No migration store or second scheduler exists.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("terminal batch repository"),
    );
    for (session_id, cursor) in [
        ("terminal-batch-anchor", Some(17)),
        ("terminal-batch-legacy", None),
    ] {
        let mut session = planned_session(
            session_id,
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("accepted before terminal")],
            }],
        );
        session
            .transition_execution(awaken_session_contract::SessionExecutionState::Terminated)
            .expect("legacy writer reached a valid terminal aggregate");
        assert!(session.terminal_cleanup.request(session_id));
        if let Some(cursor) = cursor {
            session
                .terminal_cleanup
                .freeze_targets(session_id, [], 0, cursor)
                .expect("freeze terminal visibility cursor");
        } else {
            let mut legacy = serde_json::to_value(&session.terminal_cleanup).unwrap();
            legacy["state"] = serde_json::Value::String("requested".into());
            session.terminal_cleanup = serde_json::from_value(legacy)
                .expect("legacy Requested cleanup without runtime cursor");
        }
        create(repository.as_ref(), session).await;
    }
    let mut fenced = planned_session(
        "terminal-batch-fenced",
        vec![SessionEventInput::UserMessage {
            content: vec![ContentBlock::text("accepted before quiescence")],
        }],
    );
    fenced
        .transition_execution(awaken_session_contract::SessionExecutionState::Terminated)
        .expect("terminal fence starts from a valid terminal aggregate");
    assert!(fenced.terminal_cleanup.request("terminal-batch-fenced"));
    create(repository.as_ref(), fenced).await;
    let runtime = Arc::new(EventBatchRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let report = app.reconcile_event_batches().await;
    assert!(
        report.failures.is_empty(),
        "T1/T2 root CAS succeeds: {:?}",
        report.failures
    );
    assert_eq!(report.settled, 2, "T2/T3/E3");
    for (session_id, cursor) in [
        ("terminal-batch-anchor", Some(17)),
        ("terminal-batch-legacy", None),
    ] {
        let session = repository.get(session_id).await.expect("resolved terminal");
        let entry = &session.event_batches[0].events[0];
        assert!(entry.processed, "{session_id}/E2");
        assert_eq!(
            entry
                .projection_anchor
                .map(|anchor| anchor.source_commit_cursor),
            cursor,
            "{session_id}/E3"
        );
    }
    assert!(
        !repository
            .get("terminal-batch-fenced")
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "T1/E2 Fenced is not legacy"
    );

    let mut frozen = repository.get("terminal-batch-fenced").await.unwrap();
    frozen
        .terminal_cleanup
        .freeze_targets("terminal-batch-fenced", [], 0, 23)
        .expect("T4/C3 freeze exact terminal visibility");
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
                        key: "freeze:terminal-batch-fenced".into(),
                        payload_hash,
                    },
                    payload,
                    lifecycle_facts: Vec::new(),
                },
            )
            .await
            .unwrap(),
        awaken_session_contract::SessionMutationResult::Applied { .. }
    ));
    let report = app.reconcile_event_batches().await;
    assert!(report.failures.is_empty(), "T4 root CAS succeeds");
    let frozen = repository.get("terminal-batch-fenced").await.unwrap();
    assert!(frozen.event_batches[0].events[0].processed, "T4/E3");
    assert_eq!(
        frozen.event_batches[0].events[0]
            .projection_anchor
            .map(|anchor| anchor.source_commit_cursor),
        Some(23),
        "T4/E4"
    );
    assert!(runtime.trace.lock().unwrap().is_empty(), "T1/T2/E1");
    assert!(runtime.reserved.lock().unwrap().is_empty(), "T1/T2/E1");
    assert!(runtime.outcomes.lock().unwrap().is_empty(), "T1/T2/E1");
    assert_eq!(
        runtime.tool_reply_calls.load(Ordering::SeqCst),
        0,
        "T1/T2/E1"
    );
    assert_eq!(
        runtime.primary_interrupt_calls.load(Ordering::SeqCst),
        0,
        "T1/T2/E1"
    );
}

#[tokio::test]
async fn deleting_tombstone_waits_for_terminal_batch_provenance() {
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
    session.terminal_cleanup = serde_json::from_value(serde_json::json!({
        "state": "completed",
        "effect_id": "legacy-delete-cleanup",
        "thread_ids": ["deleting-batch"],
        "delegation_watermark": 0,
        "runtime_commit_cursor": 31,
        "receipt_fingerprint": "legacy-delete-receipt"
    }))
    .expect("legacy completed terminal cleanup");
    create(repository.as_ref(), session).await;
    let runtime = Arc::new(EventBatchRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let first = app.reconcile_resource_activations().await;
    assert!(first.failures.is_empty(), "D1/E1");
    assert!(
        !repository
            .get("deleting-batch")
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "D1/E1 provenance survives the resource-first pass"
    );

    let events = app.reconcile_event_batches().await;
    assert!(events.failures.is_empty(), "D2/E2: {:?}", events.failures);
    let resolved = repository.get("deleting-batch").await.unwrap();
    assert!(resolved.event_batches[0].events[0].processed, "D2/E2");
    assert_eq!(
        resolved.event_batches[0].events[0]
            .projection_anchor
            .map(|anchor| anchor.source_commit_cursor),
        Some(31),
        "D2/E2"
    );

    let second = app.reconcile_resource_activations().await;
    assert!(second.failures.is_empty(), "D3/E3");
    assert!(matches!(
        repository.get("deleting-batch").await,
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

    app.drive_session_event_batches("receipt-priority", None)
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
    let durable = repository.get("receipt-priority").await.unwrap();
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
    app.drive_session_event_batches("receipt-priority", None)
        .await
        .expect("P3 committed System observation");
    let durable = repository.get("receipt-priority").await.unwrap();
    let reply = durable
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == reply_batch.batch_id)
        .unwrap();
    assert!(!reply.events[0].processed, "P3/E3 User remains queued");
    assert!(reply.events[2].processed, "P3/E5 exact System observed");
    assert!(runtime.reserved.lock().unwrap().is_empty(), "P1-P3/E3");
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

    app.drive_session_event_batches("outcome-interrupt-priority", Some(&interrupt.batch_id))
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
    let persisted = repository.get("outcome-interrupt-priority").await.unwrap();
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

    let interrupted = warm.drive_session_event_batches("reply-replay", None).await;
    assert!(
        interrupted
            .is_err_and(|error| error.kind == awaken_session_contract::RunErrorKind::Unavailable),
        "R1/E1"
    );
    assert!(
        !repository.get("reply-replay").await.unwrap().event_batches[0].events[0].processed,
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
    cold.drive_session_event_batches("reply-replay", None)
        .await
        .expect("R2 cold replay");
    assert_eq!(runtime.tool_reply_calls.load(Ordering::SeqCst), 2, "R2/E2");
    assert_eq!(
        runtime.staged_tool_replies.lock().unwrap().len(),
        1,
        "R2/E3"
    );
    assert!(
        repository.get("reply-replay").await.unwrap().event_batches[0].events[0].processed,
        "R2/E4"
    );
    assert_eq!(
        repository.get("reply-replay").await.unwrap().event_batches[0].events[0]
            .projection_anchor
            .map(|anchor| anchor.source_commit_cursor),
        Some(777),
        "R2/C5 freezes the answered Awaiting boundary across response loss"
    );
    cold.drive_session_event_batches("reply-replay", None)
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
        warm.drive_session_event_batches("interrupt-replay", None)
            .await
            .is_err(),
        "I1/E1"
    );
    assert!(
        !repository
            .get("interrupt-replay")
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
    cold.drive_session_event_batches("interrupt-replay", None)
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
            .get("interrupt-replay")
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "I2/E4"
    );
    cold.drive_session_event_batches("interrupt-replay", None)
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
        warm.drive_session_event_batches("interrupt-primary-replay", None)
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
            .get("interrupt-primary-replay")
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
    cold.drive_session_event_batches("interrupt-primary-replay", None)
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
            .get("interrupt-primary-replay")
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
    // resumes and settles the original batch epoch without duplicate effects.
    //
    // | Rule | User truth | System truth | Scanners | Effect |
    // | R1 | absent | none | one | reserve -> activity -> activate |
    // | R2 | Ended first User | root intent | one | reserve [System,User], activate |
    // | R3 | Running | committed | two/restart | wait, no second effects |
    // | R4 | Ended target | committed | cold | advance both and settle |
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
    app.settle_activity("initial-user-system", runtime.activity_epoch(&run_ids[0]))
        .await
        .expect("ordinary first Run settlement");

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
    restarted
        .settle_activity("initial-user-system", runtime.activity_epoch(&run_ids[1]))
        .await
        .expect("ordinary target Run settlement");
    let completed = restarted.reconcile_event_batches().await;
    assert!(completed.failures.is_empty(), "R4/E4");
    assert_eq!(completed.settled, 1, "R4/E4");
    let durable = repository
        .get("initial-user-system")
        .await
        .expect("R4 durable root");
    let batch = durable.event_batches.first().expect("R4 batch");
    assert!(batch.is_complete(), "R4/E3");
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
        .prepare(&session.session_id, prior.clone())
        .expect("G1 pending generation");
    assert_eq!(
        session
            .resources
            .revise_unattempted_pending(&session.session_id, desired.clone())
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
                    MessageId::session_event_input(&command.session_id, &command.operation_id),
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
        warm.recover_activity_for_operation("reservation-before-activity", &operation)
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
        cold.recover_activity_for_operation("reservation-before-activity", &operation)
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
        "receipt-before-policy",
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
        .get("receipt-before-policy")
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
            .get("receipt-before-policy")
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
    let before = repository.get("user-run-admission").await.unwrap().revision;
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
        repository.get("user-run-admission").await.unwrap().revision,
        before,
        "A3/E2 no root CAS"
    );

    let before_recovery = repository.get("user-run-admission").await.unwrap().revision;
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
        repository.get("user-run-admission").await.unwrap().revision,
        before_recovery,
        "A4/E3 no root CAS"
    );

    let before_completed = repository.get("user-run-admission").await.unwrap().revision;
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
        repository.get("user-run-admission").await.unwrap().revision,
        before_completed,
        "A5/E3 no root CAS"
    );
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
        app.recover_activity_for_operation("system-after-admission", &operation)
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
        .get("system-reservation-repair")
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
            .get("system-reservation-repair")
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
    cold.settle_activity("system-reservation-repair", run_epoch)
        .await
        .expect("Run activity settlement");
    let completed = cold.reconcile_event_batches().await;
    assert_eq!(completed.settled, 1, "C2/E4");
    assert!(
        repository
            .get("system-reservation-repair")
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

    let durable = repository.get("ordinary-atomic").await.unwrap();
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
            .get("ordinary-eligibility")
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "Q1/E1"
    );

    runtime.await_run(&first_run);
    warm.settle_activity("ordinary-eligibility", runtime.activity_epoch(&first_run))
        .await
        .expect("Q2 Awaiting settlement");
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

    let durable = repository.get("ordinary-eligibility").await.unwrap();
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
    let durable = repository.get("initial-outcome").await.unwrap();
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
        !repository.get("outcome-busy").await.unwrap().event_batches[0].events[0].processed,
        "B1/E2"
    );

    runtime.outcome_busy.store(false, Ordering::SeqCst);
    let prepared = app.reconcile_event_batches().await;
    assert!(prepared.failures.is_empty(), "B2/E3");
    assert!(
        repository.get("outcome-busy").await.unwrap().event_batches[0].events[0].processed,
        "B2/E3"
    );
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
    let durable = repository.get("outcome-cold-continuation").await.unwrap();
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
    drop(warm);

    let cold = application_with_runtime(
        runtime.clone(),
        repository,
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let awaiting = cold.reconcile_outcome_continuations().await;
    assert_eq!(awaiting.pending, 1, "C1/E1+E2");
    assert_eq!(runtime.outcome_continue_calls.load(Ordering::SeqCst), 1);

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

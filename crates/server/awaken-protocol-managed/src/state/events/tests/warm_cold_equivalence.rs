//! Warm/cold equivalence of the committed projection over generated schedules.
//!
//! Property: for one committed evidence prefix, the durable public history a
//! warm replica reaches by folding every intermediate prefix it happened to
//! observe equals the history a cold replica builds from that prefix alone.
//! The generator enumerates every refresh schedule (each subset of commit
//! boundaries at which the warm replica refreshes) over Session-root Runs with
//! complete durable evidence: an anchored user Event, an admitted Session
//! interval, committed transcript and lifecycle, and interval settlement.
//! Transient `evt_N` overlays are process-local by definition and excluded.
//!
//! Consumer: the timeline refactor, which must not change what either replica
//! publishes for the same evidence.

use super::*;

/// One committed boundary of a Session-root Run. Every step commits at its own
/// strictly increasing Runtime source cursor.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// Retain the user Event with its anchor, admit the Session activity it
    /// owns, commit the input message and the Run's `Running` lifecycle.
    Input(usize),
    /// Commit one assistant text response.
    Say(usize),
    /// Commit one executed tool call and its result.
    Tool(usize),
    /// Commit a tool call that awaits permission: its message, the answerable
    /// ticket, `Awaiting`, and the audit row naming the await target.
    Ask(usize),
    /// The answer resumes the Run: the ticket is consumed, `Resumed` and the
    /// tool result commit together.
    Reply(usize),
    /// Commit the final response, `Completed`, and settle the interval.
    Close(usize),
    /// Admit a Session activity with no retained user Event (a profiled or
    /// Run-admission start) and commit its Run's `Running` lifecycle.
    Begin(usize),
    /// Same admitted Run with a customer message supplied through a Run port,
    /// without a Managed Event batch (AI SDK/AG-UI input).
    ExternalInput(usize),
    /// Commit a claim-fenced `Rescheduled` transition of the same Run.
    Reschedule(usize),
    /// Commit, in one commit, a response whose tool call follows more
    /// thinking blocks than the replica holds events, its result, and a
    /// further response.
    Burst(usize),
}

struct Scenario {
    runtime: LifecycleRuntime,
    repository: Arc<awaken_session_store::SqliteManagedSessionRepository>,
    warm: ManagedState,
    session_id: String,
    cursor: u64,
    runs: Vec<Option<(RunId, u64)>>,
}

type DurableEvent = (String, serde_json::Value, Option<String>);

impl Scenario {
    async fn new(runs: usize) -> Self {
        let runtime = LifecycleRuntime::default();
        let repository = Arc::new(ephemeral_session_repo());
        let warm = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
        let session = warm
            .create_session(
                serde_json::from_value(serde_json::json!({
                    "agent":"coder", "environment_id":"env_local"
                }))
                .unwrap(),
                awaken_tenancy::DEFAULT_WORKSPACE_ID,
            )
            .await
            .unwrap();
        Self {
            runtime,
            repository,
            warm,
            session_id: session.id,
            cursor: 0,
            runs: vec![None; runs],
        }
    }

    fn next_cursor(&mut self) -> u64 {
        self.cursor += 1;
        self.cursor
    }

    fn run(&self, index: usize) -> RunId {
        self.runs[index]
            .as_ref()
            .expect("Input precedes the Run")
            .0
            .clone()
    }

    async fn apply(&mut self, step: Step) {
        let session = SessionId::from(&self.session_id);
        let cursor = self.next_cursor();
        match step {
            Step::Input(index) => {
                let retained = self
                    .warm
                    .application
                    .append_session_event_batch(
                        &self.session_id,
                        vec![SessionEventInput::UserMessage {
                            content: vec![ContentBlock::text(format!("input-{index}"))],
                        }],
                        None,
                        None,
                    )
                    .await
                    .unwrap();
                let SessionEventCommand::UserMessage { run_id, .. } = &retained.events[0].event
                else {
                    unreachable!("the retained batch holds the user message")
                };
                let run_id = run_id.clone();
                // Admission order matches production: the retained batch
                // owns the interval that its drive opens afterwards.
                let activity = self
                    .warm
                    .application
                    .begin_activity(&session)
                    .await
                    .unwrap();
                let operation_id = retained.events[0].event.operation_id().to_string();
                persist_batch_projection_anchors(
                    &self.warm,
                    &self.session_id,
                    &retained.batch_id,
                    &[cursor],
                )
                .await;
                self.runtime.push_committed_message(
                    cursor,
                    Message::new(
                        MessageId::session_event_input(&self.session_id, &operation_id),
                        Role::User,
                        vec![ContentBlock::text(format!("input-{index}"))],
                    ),
                );
                self.runtime.lifecycle.lock().unwrap().push(lifecycle(
                    cursor,
                    &self.session_id,
                    &run_id,
                    RunLifecycleEventKind::Running,
                    RunState::Running,
                ));
                self.runs[index] = Some((run_id, activity.activity_epoch));
            }
            Step::Say(index) => {
                let run = self.run(index);
                self.runtime.push_committed_message(
                    cursor,
                    Message::text(MessageId::assistant(&run, 0), Role::Assistant, "working"),
                );
            }
            Step::Tool(index) => {
                let run = self.run(index);
                let call = format!("call-{index}");
                self.runtime.push_committed_message(
                    cursor,
                    Message::new(
                        MessageId::assistant(&run, 1),
                        Role::Assistant,
                        vec![ContentBlock::tool_use(
                            &call,
                            "read",
                            serde_json::json!({"path": "notes.txt"}),
                        )],
                    ),
                );
                self.runtime.push_committed_message(
                    cursor,
                    Message::new(
                        MessageId(format!("{}-tool-result", run.0)),
                        Role::Tool,
                        vec![ContentBlock::tool_result(
                            &call,
                            vec![ContentBlock::text("contents")],
                        )],
                    ),
                );
            }
            Step::Ask(index) => {
                let run = self.run(index);
                let call = format!("ask-{index}");
                let input = serde_json::json!({"path": "secret.txt"});
                self.runtime.push_committed_message(
                    cursor,
                    Message::new(
                        MessageId::assistant(&run, 1),
                        Role::Assistant,
                        vec![ContentBlock::tool_use(&call, "read", input.clone())],
                    ),
                );
                *self.runtime.pending.lock().unwrap() = Some(Pending {
                    tool_use_id: call.clone(),
                    name: "read".into(),
                    input: input.clone(),
                    client_executed: false,
                });
                let target = awaken_agent_contract::agent::awaiting::AwaitTarget::ToolCall {
                    reason: awaken_agent_contract::agent::awaiting::ToolAwaitReason::Permission,
                    call_id: call,
                    tool: awaken_agent_contract::agent::awaiting::PendingTool {
                        tool_id: "read".into(),
                        arguments: input,
                    },
                    authority:
                        awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
                };
                let reason = target.reason();
                let draft: awaken_agent_contract::audit::draft::Draft =
                    awaken_agent_contract::audit::run_event::RunEvent::RunStateChanged {
                        state: RunState::Awaiting,
                        await_reason: Some(reason),
                        await_target: Some(target),
                    }
                    .into();
                self.runtime.audit_events.lock().unwrap().push(
                    awaken_agent_contract::audit::record::Record {
                        sequence: lifecycle_cursor(cursor).0,
                        run_id: run.clone(),
                        kind: draft.kind,
                        payload: draft.payload,
                    },
                );
                let mut awaiting = lifecycle(
                    cursor,
                    &self.session_id,
                    &run,
                    RunLifecycleEventKind::Awaiting,
                    RunState::Awaiting,
                );
                awaiting.await_reason = Some(reason);
                self.runtime.lifecycle.lock().unwrap().push(awaiting);
            }
            Step::Reply(index) => {
                let run = self.run(index);
                *self.runtime.pending.lock().unwrap() = None;
                self.runtime.push_committed_message(
                    cursor,
                    Message::new(
                        MessageId(format!("{}-ask-result", run.0)),
                        Role::Tool,
                        vec![ContentBlock::tool_result(
                            format!("ask-{index}"),
                            vec![ContentBlock::text("secret")],
                        )],
                    ),
                );
                self.runtime.lifecycle.lock().unwrap().push(lifecycle(
                    cursor,
                    &self.session_id,
                    &run,
                    RunLifecycleEventKind::Resumed,
                    RunState::Running,
                ));
            }
            Step::Begin(index) | Step::ExternalInput(index) => {
                let activity = self
                    .warm
                    .application
                    .begin_activity(&session)
                    .await
                    .unwrap();
                let run_id = RunId(format!("begun-run-{index}"));
                if matches!(step, Step::ExternalInput(_)) {
                    self.runtime.push_committed_message(
                        cursor,
                        Message::text(
                            MessageId(format!("external-input-{index}")),
                            Role::User,
                            "服务费金额没提供；收款时间没定。",
                        ),
                    );
                }
                self.runtime.lifecycle.lock().unwrap().push(lifecycle(
                    cursor,
                    &self.session_id,
                    &run_id,
                    RunLifecycleEventKind::Running,
                    RunState::Running,
                ));
                self.runs[index] = Some((run_id, activity.activity_epoch));
            }
            Step::Reschedule(index) => {
                let run = self.run(index);
                self.runtime.lifecycle.lock().unwrap().push(lifecycle(
                    cursor,
                    &self.session_id,
                    &run,
                    RunLifecycleEventKind::Rescheduled,
                    RunState::Running,
                ));
            }
            Step::Burst(index) => {
                let run = self.run(index);
                let call = format!("burst-{index}");
                let mut blocks = (0..11)
                    .map(|step| ContentBlock::thinking(format!("step {step}")))
                    .collect::<Vec<_>>();
                blocks.push(ContentBlock::tool_use(
                    &call,
                    "read",
                    serde_json::json!({"path": "burst.txt"}),
                ));
                for message in [
                    Message::new(MessageId::assistant(&run, 3), Role::Assistant, blocks),
                    Message::new(
                        MessageId(format!("{}-burst-result", run.0)),
                        Role::Tool,
                        vec![ContentBlock::tool_result(
                            &call,
                            vec![ContentBlock::text("burst")],
                        )],
                    ),
                    Message::text(MessageId::assistant(&run, 4), Role::Assistant, "after"),
                ] {
                    self.runtime.push_committed_message(cursor, message);
                }
            }
            Step::Close(index) => {
                let (run, epoch) = self.runs[index].clone().expect("Input precedes Close");
                self.runtime.push_committed_message(
                    cursor,
                    Message::text(MessageId::assistant(&run, 2), Role::Assistant, "done"),
                );
                self.runtime.lifecycle.lock().unwrap().push(lifecycle(
                    cursor,
                    &self.session_id,
                    &run,
                    RunLifecycleEventKind::Completed,
                    RunState::Ended(EndCause::NaturalEnd),
                ));
                self.warm
                    .application
                    .settle_activity_observed(
                        &session,
                        epoch,
                        Some(
                            awaken_session_state_contract::SessionRuntimeIntervalObservation {
                                activity_epoch: epoch,
                                thread_id: ThreadId(self.session_id.clone()),
                                run_id: run,
                                lifecycle_cursor: lifecycle_cursor(cursor),
                                source_commit_cursor: cursor,
                            },
                        ),
                    )
                    .await
                    .unwrap();
            }
        }
    }

    fn durable_history(state: &ManagedState, session_id: &str) -> Vec<DurableEvent> {
        state
            .list_events(session_id, None, None, false)
            .unwrap()
            .data
            .into_iter()
            .filter(|event| !is_transient_event_id(&event.id))
            .map(|event| {
                (
                    event.id,
                    serde_json::to_value(&event.kind).unwrap(),
                    event.processed_at,
                )
            })
            .collect()
    }

    /// Refresh the warm replica on its own history, then build a fresh cold
    /// replica over the same stores and compare the published results.
    async fn assert_warm_equals_cold(&self, label: &str) -> Vec<DurableEvent> {
        self.warm
            .refresh_committed_events(&self.session_id)
            .await
            .unwrap();
        let cold =
            ManagedState::new(self.runtime.clone()).with_session_repo(self.repository.clone());
        cold.refresh_committed_events(&self.session_id)
            .await
            .unwrap();
        let warm = Self::durable_history(&self.warm, &self.session_id);
        assert_eq!(
            warm,
            Self::durable_history(&cold, &self.session_id),
            "{label}: warm and cold replicas diverge on one evidence prefix"
        );
        assert_one_aggregate_opening_per_interval(&warm, label);
        assert_eq!(
            self.warm.get_session(&self.session_id).unwrap().status,
            cold.get_session(&self.session_id).unwrap().status,
            "{label}: aggregate status diverges"
        );
        warm
    }
}

/// The aggregate opens once per interval: a second `session.status_running`
/// before the aggregate closes would announce one interval twice.
fn assert_one_aggregate_opening_per_interval(history: &[DurableEvent], label: &str) {
    let mut open = false;
    for (id, kind, _) in history {
        match kind["type"].as_str().unwrap_or_default() {
            "session.status_running" => {
                assert!(!open, "{label}: {id} opens an interval already open");
                open = true;
            }
            "session.status_idle" | "session.status_terminated" => open = false,
            _ => {}
        }
    }
}

const RUN_WITH_EXECUTED_TOOL: [&str; 10] = [
    "user.message",
    "session.status_running",
    "session.thread_status_running",
    "agent.message",
    "agent.tool_use",
    "agent.tool_result",
    "agent.message",
    "session.thread_status_idle",
    "session.usage",
    "session.status_idle",
];

async fn assert_every_schedule_is_equivalent(steps: &[Step], runs: usize, expected: &[&str]) {
    assert!(steps.len() < usize::BITS as usize);
    let mut final_histories = std::collections::BTreeSet::new();
    for schedule in 0_usize..(1 << steps.len()) {
        let mut scenario = Scenario::new(runs).await;
        for (position, step) in steps.iter().copied().enumerate() {
            scenario.apply(step).await;
            if schedule & (1 << position) != 0 {
                scenario
                    .assert_warm_equals_cold(&format!("schedule {schedule:#b} after {step:?}"))
                    .await;
            }
        }
        let history = scenario
            .assert_warm_equals_cold(&format!("schedule {schedule:#b} final"))
            .await;
        // Session and Run identities are minted per scenario, so compare the
        // schedule-independent shape: every schedule reaches one wire sequence.
        final_histories.insert(
            history
                .iter()
                .map(|(_, kind, _)| kind["type"].as_str().unwrap_or_default().to_string())
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(
        final_histories.into_iter().collect::<Vec<_>>(),
        vec![expected.to_vec()],
        "every refresh schedule converges on the one expected public sequence"
    );
}

#[tokio::test]
async fn every_refresh_schedule_of_one_run_equals_its_cold_rebuild() {
    // Causes: C1 the warm replica refreshes after any subset of the commit
    // boundaries of one Run (input, response, executed tool, completion);
    // C2 a cold replica is built at each of those points. Effect: E1 both
    // publish the identical durable history (ids, payloads, completion time)
    // and aggregate status; E2 all 16 schedules end on one public sequence.
    assert_every_schedule_is_equivalent(
        &[Step::Input(0), Step::Say(0), Step::Tool(0), Step::Close(0)],
        1,
        &RUN_WITH_EXECUTED_TOOL,
    )
    .await;
}

#[tokio::test]
async fn external_customer_input_survives_every_refresh_schedule_and_cold_rebuild() {
    // C1 a Run-port input has no Managed admission batch; C2 warm projection
    // refreshes at any subset of its three commits; C3 a fresh cold adapter
    // reads the same stores. R1 all eight schedules -> exactly one public
    // customer input with stable ID/bytes/time, before the assistant text;
    // R2 C2+C3 -> identical complete history, including terminal state.
    // The sibling Input scenarios retain command-owned receipt deduplication.
    assert_every_schedule_is_equivalent(
        &[Step::ExternalInput(0), Step::Say(0), Step::Close(0)],
        1,
        &[
            "session.status_running",
            "session.thread_status_running",
            "user.message",
            "agent.message",
            "agent.message",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle",
        ],
    )
    .await;
}

#[tokio::test]
async fn every_refresh_schedule_of_consecutive_runs_equals_its_cold_rebuild() {
    // Causes: C1 as above over two consecutive Runs, each its own admitted
    // Session interval; C2 a refresh may observe the first interval closed and
    // the second opened in one read, or every boundary separately. Effects:
    // E1/E2 as above across all 128 schedules, including the second interval's
    // aggregate opening and closing edges.
    assert_every_schedule_is_equivalent(
        &[
            Step::Input(0),
            Step::Say(0),
            Step::Tool(0),
            Step::Close(0),
            Step::Input(1),
            Step::Say(1),
            Step::Close(1),
        ],
        2,
        &[
            &RUN_WITH_EXECUTED_TOOL[..],
            &[
                "user.message",
                "session.status_running",
                "session.thread_status_running",
                "agent.message",
                "agent.message",
                "session.thread_status_idle",
                "session.usage",
                "session.status_idle",
            ],
        ]
        .concat(),
    )
    .await;
}

#[tokio::test]
async fn every_refresh_schedule_of_a_permission_pause_equals_its_cold_rebuild() {
    // Causes: C1 a Run pauses for permission inside its admitted interval
    // (ticket + Awaiting + audited await target) and resumes before it
    // completes; C2 the warm replica may observe the pause while its ticket is
    // answerable, only after the answer consumed it, or never. Effects: E1 the
    // cold replica reconstructs the same ask and Thread pause from the audited
    // target alone; E2 all 16 schedules end on one public sequence.
    assert_every_schedule_is_equivalent(
        &[Step::Input(0), Step::Ask(0), Step::Reply(0), Step::Close(0)],
        1,
        &[
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.tool_use",
            "session.thread_status_idle",
            "session.thread_status_running",
            "agent.tool_result",
            "agent.message",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle",
        ],
    )
    .await;
}

const RESCHEDULED_RUN: [&str; 9] = [
    "session.status_running",
    "session.thread_status_running",
    "session.status_rescheduled",
    "session.thread_status_rescheduled",
    "session.thread_status_running",
    "agent.message",
    "session.thread_status_idle",
    "session.usage",
    "session.status_idle",
];

#[tokio::test]
async fn every_refresh_schedule_of_a_rescheduled_run_equals_its_cold_rebuild() {
    // Causes: C1 a Session activity is admitted with no retained user Event, so
    // no Event batch names its Run; C2 the Run commits Running, then a
    // claim-fenced Rescheduled transition, inside that one interval; C3 the
    // warm replica refreshes after any subset of the boundaries while the
    // interval is open or after it closes. Effects: E1 warm and cold publish
    // one history at every point, including while the interval is open; E2 the
    // aggregate opens once, at the Run's first opening, and a reschedule does
    // not reopen it; E3 all 8 schedules end on the sequence a user-Event Run
    // with the same reschedule publishes.
    assert_every_schedule_is_equivalent(
        &[Step::Begin(0), Step::Reschedule(0), Step::Close(0)],
        1,
        &RESCHEDULED_RUN,
    )
    .await;
}

#[tokio::test]
async fn every_refresh_schedule_of_an_open_rescheduled_interval_equals_its_cold_rebuild() {
    // Causes: as above, but the interval is still open at the final read.
    // Effects: E1/E2 as above; E3 every schedule ends on the open prefix.
    assert_every_schedule_is_equivalent(
        &[Step::Begin(0), Step::Reschedule(0)],
        1,
        &RESCHEDULED_RUN[..5],
    )
    .await;
}

#[tokio::test]
async fn every_refresh_schedule_of_an_open_user_event_interval_equals_its_cold_rebuild() {
    // Causes: a user-Event Run is observed only while its interval is open.
    // Effects: warm and cold agree at every point and end on one open prefix.
    assert_every_schedule_is_equivalent(
        &[Step::Input(0), Step::Say(0), Step::Tool(0)],
        1,
        &RUN_WITH_EXECUTED_TOOL[..6],
    )
    .await;
}

#[tokio::test]
async fn every_refresh_schedule_of_a_rescheduled_second_interval_equals_its_cold_rebuild() {
    // Causes: C1 two consecutive admitted intervals with no retained user
    // Event; C2 the second Run reschedules inside its interval, so its
    // interval holds two opening commits after the first interval's close.
    // Effects: E1 warm and cold agree at every point; E2 each interval opens
    // the aggregate once, at its own first opening, whether it is read live or
    // only after it closed; E3 all 32 schedules end on one sequence.
    assert_every_schedule_is_equivalent(
        &[
            Step::Begin(0),
            Step::Close(0),
            Step::Begin(1),
            Step::Reschedule(1),
            Step::Close(1),
        ],
        2,
        &[
            &[
                "session.status_running",
                "session.thread_status_running",
                "agent.message",
                "session.thread_status_idle",
                "session.usage",
                "session.status_idle",
            ][..],
            &RESCHEDULED_RUN[..],
        ]
        .concat(),
    )
    .await;
}

#[tokio::test]
async fn every_refresh_schedule_orders_one_commits_messages_by_their_own_coordinates() {
    // Causes: C1 one commit holds a response whose tool call is its twelfth
    // block, then that call's result, then another response; C2 the replica
    // holds fewer events than that block index when it orders the commit, and
    // a different number under every refresh schedule. Effects: E1 warm and
    // cold agree at every point; E2 every schedule orders the commit by each
    // fact's own Message and block, so the call precedes its result and the
    // later response.
    assert_every_schedule_is_equivalent(
        &[Step::Input(0), Step::Burst(0), Step::Close(0)],
        1,
        &[
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.thinking",
            "agent.tool_use",
            "agent.tool_result",
            "agent.message",
            "agent.message",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle",
        ],
    )
    .await;
}
